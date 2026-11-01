//! User approval of temporary network access and network rule changes.
//!
//! Asks the user on the host to allow or deny a request from the sandbox:
//! run one command with network access to more hosts or with an on-demand
//! rule, or add or change a network rule. The request shows as a short
//! desktop notification with allow and deny buttons. On macOS, a small
//! notifier app shows the notification, and it can also show all details
//! of the request in a window. On Linux, a window with all details opens
//! from the notification when the host has an X display. When the
//! notification cannot show, the details window asks on Linux, and a
//! system dialog asks on macOS. When the host shows nothing and the
//! monitor TUI runs, a dialog in the TUI asks last. It gets only the time
//! that is left. The text from the sandbox is cleaned and cut before it
//! shows. No answer in time, or no way to ask, means deny. A late answer
//! has no effect.

#[cfg(not(target_os = "macos"))]
pub(crate) mod dialog;
#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
// The tests check the notifier app protocol and install on all hosts.
#[cfg(all(test, not(target_os = "macos")))]
mod macos {
    mod notifier;
    mod protocol;
}
mod monitor;
pub(crate) mod text;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

pub(crate) use monitor::MonitorSlot;
use text::PromptText;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::config::config_values::NetworkRule;
use crate::data_dir::DataDir;
use crate::messages::grants as msg;

/// Request that the user must approve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantPrompt {
    /// Name of the sandbox (project) that asks.
    pub sandbox: String,
    /// What the sandbox asks for.
    pub request: PromptRequest,
}

/// What the sandbox asks for. All text is untrusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptRequest {
    /// Run one command with access to these host patterns. A single `*`
    /// asks for all public hosts.
    Hosts {
        /// Host patterns.
        hosts: Vec<String>,
        /// Command to run.
        command: PromptCommand,
    },
    /// Run one command with this on-demand rule.
    Rule {
        /// Name of the rule.
        name: String,
        /// The rule as it applies now.
        rule: NetworkRule,
        /// Command to run.
        command: PromptCommand,
    },
    /// Add or change a network rule.
    RuleChange {
        /// Name of the rule.
        name: String,
        /// The rule before the change, as it applies now. `None` for a new
        /// rule.
        old: Option<NetworkRule>,
        /// The rule after the change, as it applies after the change.
        rule: NetworkRule,
        /// Other keys of the rule table in the project file, as `(key,
        /// value)` text. The rule does not use them. The change keeps them.
        other: Vec<(String, String)>,
    },
}

/// Command of an access request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptCommand {
    /// Shell command.
    pub cmd: String,
    /// Working directory in the sandbox.
    pub cwd: String,
    /// More environment of the command, as `KEY=VALUE` entries.
    pub env: Vec<String>,
    /// Maximum duration of the access, in seconds.
    pub duration_secs: u32,
}

/// Decision of the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The user allowed the request.
    Allow,
    /// The user denied the request.
    DeniedByUser,
    /// The request is denied for another reason. The text tells why (timeout,
    /// no notification support).
    Deny(String),
}

/// Decision of the user, and the notice that shows while the approved
/// command runs.
#[derive(Debug)]
pub struct Approval {
    /// Decision of the user.
    pub decision: Decision,
    /// Notice with an abort button, if the host can show one. Only an
    /// allowed command request has it.
    pub running: Option<Running>,
}

impl From<Decision> for Approval {
    fn from(decision: Decision) -> Self {
        Self {
            decision,
            running: None,
        }
    }
}

/// Notice that shows while an approved command runs. The user can abort
/// the command from it. The notice goes away when this value drops.
#[derive(Debug)]
pub struct Running {
    abort: Option<oneshot::Receiver<()>>,
    _close: oneshot::Sender<()>,
}

/// Platform side of a [`Running`] notice.
pub(crate) struct RunningControl {
    /// Send when the user selects abort.
    pub abort: oneshot::Sender<()>,
    /// Closes when the notice must go away.
    pub close: oneshot::Receiver<()>,
}

impl Running {
    /// Make a notice and its platform side.
    pub(crate) fn new() -> (Self, RunningControl) {
        let (abort_tx, abort_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel();
        let running = Self {
            abort: Some(abort_rx),
            _close: close_tx,
        };
        let control = RunningControl {
            abort: abort_tx,
            close: close_rx,
        };
        (running, control)
    }

    /// Take the abort signal. It resolves with `Ok` when the user selects
    /// abort, and with `Err` when the notice goes away with no abort.
    pub fn take_abort(&mut self) -> Option<oneshot::Receiver<()>> {
        self.abort.take()
    }
}

/// Asks the user for a decision.
pub trait Approver {
    /// Show the request to the user and wait for the decision.
    /// Args:
    ///  - `prompt`: Request to show
    ///  - `timeout`: Maximum wait time. When it ends, the decision is deny.
    fn approve(
        &self,
        prompt: GrantPrompt,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Approval> + 'static>>;
}

/// Future of a system ask.
type AskFuture = Pin<Box<dyn Future<Output = Asked> + 'static>>;

/// Shows the clean prompt text on the host desktop and waits for the
/// answer in the time limit.
type SystemAsk = Rc<dyn Fn(PromptText, Duration) -> AskFuture>;

/// Result of a system ask.
enum Asked {
    /// The request showed. The approval holds the decision, also a timeout
    /// or a dismissed notification.
    Shown(Approval),
    /// The host showed nothing. The text tells why.
    Unavailable(String),
}

/// Get the approver for this host: a desktop notification. On macOS, a
/// system dialog when the notification cannot show. When nothing can show,
/// the monitor TUI asks if it runs.
/// Args:
///  - `data`: Data directory. On macOS, the notifier app installs there.
///  - `monitor`: Place of the monitor TUI dialog. Empty when no TUI runs.
pub(crate) fn system(data: &DataDir, monitor: MonitorSlot) -> Rc<dyn Approver> {
    #[cfg(target_os = "macos")]
    let system: SystemAsk = {
        let data = data.clone();
        Rc::new(move |text, timeout| -> AskFuture {
            let data = data.clone();
            Box::pin(async move { macos::ask(text, timeout, &data).await })
        })
    };
    #[cfg(not(target_os = "macos"))]
    let system: SystemAsk = {
        let _ = data;
        Rc::new(|text, timeout| -> AskFuture { Box::pin(linux::ask(text, timeout)) })
    };
    Rc::new(SystemApprover { system, monitor })
}

/// Approver that asks the user on the host desktop, and in the monitor TUI
/// when the desktop cannot show the request.
struct SystemApprover {
    /// Ask on the host desktop.
    system: SystemAsk,
    /// Place of the monitor TUI dialog.
    monitor: MonitorSlot,
}

impl Approver for SystemApprover {
    fn approve(
        &self,
        prompt: GrantPrompt,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Approval> + 'static>> {
        let system = self.system.clone();
        let monitor = self.monitor.clone();
        Box::pin(async move {
            let text = log_pending(&prompt, timeout);
            let sandbox = text.sandbox.clone();
            log_decision(&sandbox, decide(text, timeout, &system, &monitor).await)
        })
    }
}

/// Ask the user in order of priority: the host desktop first, then the
/// monitor TUI. When neither can show the request, deny it.
/// Args:
///  - `text`: Clean prompt text
///  - `timeout`: Maximum wait time for all asks together
///  - `system`: Ask on the host desktop
///  - `monitor`: Place of the monitor TUI dialog
///
/// Returns:
///   Decision of the user and the running notice. Only the host desktop
///   shows the running notice.
async fn decide(
    text: PromptText,
    timeout: Duration,
    system: &SystemAsk,
    monitor: &MonitorSlot,
) -> Approval {
    let deadline = Instant::now() + timeout;
    let sandbox = text.sandbox.clone();
    let why = match system(text.clone(), timeout).await {
        Asked::Shown(approval) => {
            tracing::debug!("grants: {sandbox}: the host desktop asked the user");
            return approval;
        }
        Asked::Unavailable(why) => why,
    };
    let Some(dialog) = monitor.get() else {
        return Decision::Deny(why).into();
    };
    // The monitor gets only the time that is left. With no time left, do
    // not show a dialog that closes at once.
    if Instant::now() >= deadline {
        return Decision::Deny(msg::TIMEOUT.into()).into();
    }
    tracing::info!("grants: {sandbox}: {why}; asking in the monitor TUI");
    let decision = monitor::ask(&*dialog, &text, deadline).await;
    tracing::debug!("grants: {sandbox}: the monitor TUI asked the user");
    decision.into()
}

/// Make the clean text of the request and log that it waits for the user.
fn log_pending(prompt: &GrantPrompt, timeout: Duration) -> PromptText {
    let text = PromptText::new(prompt);
    // Printing to the terminal would break the screen of the agent that
    // runs in it. Thus the line goes only to the log.
    tracing::info!(
        "{}",
        crate::messages::grants::pending(&text.sandbox, &text.title, timeout.as_secs())
    );
    text
}

/// Log the decision of the user and return the approval.
fn log_decision(sandbox: &str, approval: Approval) -> Approval {
    match &approval.decision {
        Decision::Allow => tracing::info!("grants: {sandbox}: user allowed the request"),
        Decision::DeniedByUser => tracing::info!("grants: {sandbox}: user denied the request"),
        Decision::Deny(why) => tracing::info!("grants: {sandbox}: request denied: {why}"),
    }
    approval
}

#[cfg(test)]
mod tests {
    //! Tests for the priority of the ways to ask the user.

    use std::cell::Cell;

    use airlock_monitor::{ApprovalDiffKind, ApprovalRequest};
    use tokio::sync::mpsc;

    use super::monitor::MonitorDialog;
    use super::*;
    use crate::config::config_values::{NetworkRule, RuleEnabled};

    /// Time limit of the tests that do not wait for it to end.
    const LONG: Duration = Duration::from_secs(30);
    /// Time limit of the tests that wait for it to end.
    const SHORT: Duration = Duration::from_millis(200);

    /// Monitor dialog that gives each request to the test.
    struct ChannelDialog(mpsc::UnboundedSender<ApprovalRequest>);

    impl MonitorDialog for ChannelDialog {
        fn show(&self, request: ApprovalRequest) -> Result<(), ApprovalRequest> {
            self.0.send(request).map_err(|e| e.0)
        }
    }

    /// Monitor dialog of a TUI that stopped.
    struct StoppedDialog;

    impl MonitorDialog for StoppedDialog {
        fn show(&self, request: ApprovalRequest) -> Result<(), ApprovalRequest> {
            Err(request)
        }
    }

    /// Make a monitor slot with a dialog that gives the requests to the
    /// returned receiver.
    fn monitor() -> (MonitorSlot, mpsc::UnboundedReceiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let slot = MonitorSlot::default();
        slot.set(Rc::new(ChannelDialog(tx)));
        (slot, rx)
    }

    /// Make a system ask that waits `delay` and then gives `asked`. Counts
    /// the calls in `calls`.
    fn system_ask(asked: fn() -> Asked, delay: Duration, calls: Rc<Cell<u32>>) -> SystemAsk {
        Rc::new(move |_, _| -> AskFuture {
            calls.set(calls.get() + 1);
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                asked()
            })
        })
    }

    /// System ask result when the host desktop cannot show the request.
    fn unavailable() -> Asked {
        Asked::Unavailable("no notification service".into())
    }

    /// Text of a new rule request. Its details have a diff.
    fn rule_change() -> PromptText {
        PromptText::new(&GrantPrompt {
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
        })
    }

    /// Test that the monitor TUI asks when the host desktop cannot show
    /// the request, and that its answer is the decision. The TUI must get
    /// the full details, also the diff of a rule change.
    ///   1. Make a system ask that is unavailable and a monitor dialog
    ///   2. Ask for a rule change and allow it in the dialog
    ///   3. Check the request text, the diff lines and the allow decision
    ///   4. Ask again, deny it in the dialog and check the user deny
    #[tokio::test]
    async fn unavailable_system_with_monitor_uses_monitor_answer() {
        let system = system_ask(unavailable, Duration::ZERO, Rc::default());
        let (slot, mut requests) = monitor();
        let text = rule_change();

        let answer = async {
            let request = requests.recv().await.unwrap();
            let shown = (
                request.title.clone(),
                request.body.clone(),
                request.details.clone(),
            );
            request.reply.send(true).unwrap();
            shown
        };
        let (approval, (title, body, details)) =
            tokio::join!(decide(text.clone(), LONG, &system, &slot), answer);
        assert_eq!(approval.decision, Decision::Allow);
        assert!(approval.running.is_none());
        assert_eq!(title, text.title);
        assert_eq!(body, text.body);
        assert_eq!(details.len(), text.details.len());
        let diff = details
            .iter()
            .find_map(|d| d.diff.clone())
            .expect("rule change has a diff");
        assert!(!diff.is_empty());
        assert!(diff.iter().all(|l| l.kind == ApprovalDiffKind::Add));
        assert!(diff.iter().any(|l| l.text.contains("registry.npmjs.org")));

        let answer = async {
            let request = requests.recv().await.unwrap();
            request.reply.send(false).unwrap();
        };
        let (approval, ()) = tokio::join!(decide(text, LONG, &system, &slot), answer);
        assert_eq!(approval.decision, Decision::DeniedByUser);
    }

    /// Test that the request is denied with the reason of the host desktop
    /// when no monitor TUI runs or the TUI stopped.
    ///   1. Ask with an unavailable system ask and an empty slot
    ///   2. Check the deny with the system reason
    ///   3. Ask again with the dialog of a stopped TUI
    ///   4. Check the deny that tells that the TUI stopped
    #[tokio::test]
    async fn unavailable_system_without_monitor_denies() {
        let system = system_ask(unavailable, Duration::ZERO, Rc::default());
        let approval = decide(rule_change(), LONG, &system, &MonitorSlot::default()).await;
        assert_eq!(
            approval.decision,
            Decision::Deny("no notification service".into())
        );

        let slot = MonitorSlot::default();
        slot.set(Rc::new(StoppedDialog));
        let approval = decide(rule_change(), LONG, &system, &slot).await;
        assert_eq!(
            approval.decision,
            Decision::Deny(msg::MONITOR_STOPPED.into())
        );
    }

    /// Test that the monitor TUI does not ask when the host desktop showed
    /// the request. A timeout or a dismiss there is also a decision.
    ///   1. Make a system ask that gives a timeout deny
    ///   2. Ask with a monitor dialog
    ///   3. Check the system decision and that the dialog got no request
    #[tokio::test]
    async fn shown_system_does_not_use_monitor() {
        let calls = Rc::new(Cell::new(0));
        let system = system_ask(
            || Asked::Shown(Decision::Deny(msg::TIMEOUT.into()).into()),
            Duration::ZERO,
            calls.clone(),
        );
        let (slot, mut requests) = monitor();
        let approval = decide(rule_change(), LONG, &system, &slot).await;
        assert_eq!(approval.decision, Decision::Deny(msg::TIMEOUT.into()));
        assert_eq!(calls.get(), 1);
        assert!(requests.try_recv().is_err());
    }

    /// Test that the monitor TUI gets only the time that the system ask
    /// left, and that the dialog closes when the time ends. A late answer
    /// must have no effect.
    ///   1. Ask with a fast unavailable system ask and a monitor dialog
    ///   2. Do not answer and check the timeout deny
    ///   3. Check that the reply channel of the dialog is closed
    ///   4. Ask with a system ask that uses all the time
    ///   5. Check the timeout deny and that the dialog got no request
    #[tokio::test(start_paused = true)]
    async fn monitor_timeout_denies_and_closes_dialog() {
        let system = system_ask(unavailable, Duration::ZERO, Rc::default());
        let (slot, mut requests) = monitor();
        let approval = decide(rule_change(), SHORT, &system, &slot).await;
        assert_eq!(approval.decision, Decision::Deny(msg::TIMEOUT.into()));
        let request = requests.try_recv().unwrap();
        assert!(request.reply.is_closed());

        let slow = system_ask(unavailable, SHORT, Rc::default());
        let approval = decide(rule_change(), SHORT, &slow, &slot).await;
        assert_eq!(approval.decision, Decision::Deny(msg::TIMEOUT.into()));
        assert!(requests.try_recv().is_err());
    }
}
