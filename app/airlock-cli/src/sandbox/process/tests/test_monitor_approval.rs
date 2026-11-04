//! Tests for approval requests in the monitor of a sandbox process client.
//! The server asks, the request goes over a real Unix socket to the monitor
//! client, and a fake TUI dialog on the client side answers.

use std::rc::Rc;
use std::time::Duration;

use airlock_monitor::{ApprovalDiffKind, ApprovalRequest};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::approval::{
    self, Approval, Decision, GrantPrompt, MonitorDialog, MonitorSlot, PromptRequest,
};
use crate::config::config_values::{NetworkRule, RuleEnabled};
use crate::messages::grants as msg;
use crate::runtime::{MonitorSource, subscribe};
use crate::settings::ApprovalDialogs;
use crate::test_cfg::cli_server::{CliClient, CliSandbox, LoopbackGuest};
use crate::test_cfg::oci::data_dir_at;
use crate::test_cfg::{block_on_local, eventually_async, temp_dir};

/// Time limit of the asks that the test answers.
const LONG: Duration = Duration::from_secs(30);

/// Time limit of the asks that the test does not answer.
const SHORT: Duration = Duration::from_millis(300);

/// Fake TUI dialog. It gives each request to the test.
struct ChannelDialog(mpsc::UnboundedSender<ApprovalRequest>);

impl MonitorDialog for ChannelDialog {
    fn show(&self, request: ApprovalRequest, _: Instant) -> Result<(), ApprovalRequest> {
        self.0.send(request).map_err(|e| e.0)
    }
}

/// Request for a new on-demand rule. Its details have a diff.
fn rule_change() -> GrantPrompt {
    GrantPrompt {
        sandbox: "my-proj".into(),
        request: PromptRequest::RuleChange {
            name: "npm".into(),
            old: None,
            rule: NetworkRule {
                enabled: RuleEnabled::OnDemand,
                description: String::new(),
                allow: vec!["registry.npmjs.org".into()],
                deny: vec![],
                passthrough: false,
                inject: vec![],
            },
            other: vec![],
        },
    }
}

/// Serve the CLI socket of a sandbox process, and attach a monitor with a
/// fake TUI dialog.
/// Returns:
///   The sandbox, the client connection, the monitor source and the
///   requests that the fake dialog gets.
async fn attach() -> (
    CliSandbox,
    CliClient,
    MonitorSource,
    mpsc::UnboundedReceiver<ApprovalRequest>,
) {
    let sandbox = CliSandbox::start("", false, LoopbackGuest::client()).await;
    let client = CliClient::connect(&sandbox.sock).await;
    let (source, requests) = attach_monitor(&client).await;
    (sandbox, client, source, requests)
}

/// Attach a monitor with a fake TUI dialog over `client`.
/// Returns:
///   The monitor source and the requests that the fake dialog gets.
async fn attach_monitor(
    client: &CliClient,
) -> (MonitorSource, mpsc::UnboundedReceiver<ApprovalRequest>) {
    let (tx, requests) = mpsc::unbounded_channel();
    let dialog = MonitorSlot::default();
    dialog.set(Rc::new(ChannelDialog(tx)));
    let source = subscribe(&client.service, dialog).await.unwrap();
    (source, requests)
}

/// Answer the next request that the fake dialog gets.
async fn answer_next(requests: &mut mpsc::UnboundedReceiver<ApprovalRequest>, allow: bool) {
    let request = requests.recv().await.unwrap();
    request.reply.send(allow).unwrap();
}

/// Ask for the rule change with the approver of the `monitor` setting on
/// the server. This setting does not use the host desktop.
async fn ask(sandbox: &CliSandbox, timeout: Duration) -> Approval {
    let dir = temp_dir();
    let data = data_dir_at(dir.path());
    approval::for_settings(
        ApprovalDialogs::Monitor,
        &data,
        Some(sandbox.approvals.clone()),
    )
    .approve(rule_change(), timeout)
    .await
}

/// Test that an approval request of the sandbox process shows in the monitor
/// dialog of the client, and that the answer of the user comes back. The
/// dialog must get the full details, also the diff. After the monitor
/// detaches, the server has no dialog.
///   1. Attach a monitor with a fake dialog
///   2. Ask on the server and allow the request in the dialog
///   3. Check the request text, the diff lines and the allow decision
///   4. Ask again, deny it in the dialog and check the user deny
///   5. Drop the monitor source and check that the server slot is empty
///   6. Ask again and check the deny that tells that no monitor is open
#[test]
fn approval_request_crosses_rpc_and_answer_returns() {
    block_on_local(async {
        let (sandbox, _client, source, mut requests) = attach().await;
        assert!(sandbox.approvals.get().is_some());

        let answer = async {
            let request = requests.recv().await.unwrap();
            let shown = (request.title.clone(), request.details.clone());
            request.reply.send(true).unwrap();
            shown
        };
        let (approval, (title, details)) = tokio::join!(ask(&sandbox, LONG), answer);
        assert_eq!(approval.decision, Decision::Allow);
        assert!(title.contains("npm"));
        assert!(
            details
                .iter()
                .any(|d| d.label == msg::LABEL_SANDBOX && d.value == "my-proj")
        );
        let diff = details
            .iter()
            .find_map(|d| d.diff.clone())
            .expect("rule change has a diff");
        assert!(diff.iter().all(|l| l.kind == ApprovalDiffKind::Add));
        assert!(diff.iter().any(|l| l.text.contains("registry.npmjs.org")));

        let answer = async {
            let request = requests.recv().await.unwrap();
            request.reply.send(false).unwrap();
        };
        let (approval, ()) = tokio::join!(ask(&sandbox, LONG), answer);
        assert_eq!(approval.decision, Decision::DeniedByUser);

        drop(source);
        eventually_async("monitor dialog removed", async || {
            sandbox.approvals.get().is_none().then_some(())
        })
        .await;
        let approval = ask(&sandbox, LONG).await;
        assert_eq!(
            approval.decision,
            Decision::Deny(msg::MONITOR_NOT_OPEN.into())
        );
    });
}

/// Test that the dialog of the client closes when the time limit of the
/// server ends, so that a late answer has no effect.
///   1. Attach a monitor with a fake dialog
///   2. Ask on the server with a short time limit and do not answer
///   3. Check the timeout deny
///   4. Check that the reply channel of the dialog closes
#[test]
fn approval_timeout_closes_monitor_dialog() {
    block_on_local(async {
        let (sandbox, _client, _source, mut requests) = attach().await;
        let approval = ask(&sandbox, SHORT).await;
        assert_eq!(approval.decision, Decision::Deny(msg::TIMEOUT.into()));

        let request = requests.recv().await.unwrap();
        eventually_async("dialog reply closed", async || {
            request.reply.is_closed().then_some(())
        })
        .await;
    });
}

/// Test that the monitor of an older client asks again when a newer client
/// detaches. The newest client asks, but its detach must not remove the
/// dialogs of the other clients.
///   1. Attach monitor A, then monitor B on a second connection
///   2. Ask on the server and check that B gets the request
///   3. Drop monitor B and wait until the server removes its dialog
///   4. Ask again and check that A gets the request and its answer returns
#[test]
fn newer_monitor_detach_gives_approvals_back_to_older_monitor() {
    block_on_local(async {
        let (sandbox, _client_a, _source_a, mut requests_a) = attach().await;
        let client_b = CliClient::connect(&sandbox.sock).await;
        let (source_b, mut requests_b) = attach_monitor(&client_b).await;
        assert_eq!(sandbox.approvals.len(), 2);

        let (approval, ()) = tokio::join!(ask(&sandbox, LONG), answer_next(&mut requests_b, true));
        assert_eq!(approval.decision, Decision::Allow);

        drop(source_b);
        eventually_async("monitor B dialog removed", async || {
            (sandbox.approvals.len() == 1).then_some(())
        })
        .await;
        let (approval, ()) =
            tokio::join!(ask(&sandbox, LONG), answer_next(&mut requests_a, false));
        assert_eq!(approval.decision, Decision::DeniedByUser);
    });
}

/// Test that a request waits for the local dialog when the client attached
/// the monitor before its TUI started. The client attaches the feed first
/// and starts the TUI later, so the request must not fail at once.
///   1. Attach a monitor with an empty dialog place
///   2. Ask on the server
///   3. After a short time, register a fake dialog in the place
///   4. Allow the request in the dialog and check the allow decision
#[test]
fn approval_request_before_local_dialog_starts_waits_for_dialog() {
    block_on_local(async {
        let sandbox = CliSandbox::start("", false, LoopbackGuest::client()).await;
        let client = CliClient::connect(&sandbox.sock).await;
        let local = MonitorSlot::default();
        let _source = subscribe(&client.service, local.clone()).await.unwrap();

        let answer = async {
            // Give the request time to arrive at the client before the
            // dialog registers.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let (tx, mut requests) = mpsc::unbounded_channel();
            local.set(Rc::new(ChannelDialog(tx)));
            // With no wait on the client, the request ends before the
            // dialog gets it. Then stop, so the decision check fails.
            let answer = answer_next(&mut requests, true);
            let _ = tokio::time::timeout(Duration::from_secs(5), answer).await;
        };
        let (approval, ()) = tokio::join!(ask(&sandbox, LONG), answer);
        assert_eq!(approval.decision, Decision::Allow);
    });
}
