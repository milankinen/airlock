//! Tests for temporary network grants: the request check, the user
//! approval, the grant proxy policy and the end of a grant.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use airlock_common::network_capnp::{grant_result, network_proxy, upsert_result};
use airlock_test_utils::temp_dir;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use crate::approval::{
    Approval, Approver, Decision, GrantPrompt, PromptCommand, PromptRequest, Running,
    RunningControl,
};
use crate::config::config_values::RuleEnabled;
use crate::messages::grants as msg;
use crate::network::grant::GrantAccess;
use crate::network::target::NetworkTarget;
use crate::network::{GrantContext, Network};
use crate::test_cfg::network::*;
use crate::util::VerifiedPath;

/// Approver that gives a fixed decision and keeps the prompts. An allowed
/// command request gets a running notice. The test keeps its platform side.
/// The test can set an action that runs while the user decides.
struct FakeApprover {
    decision: Decision,
    prompts: RefCell<Vec<GrantPrompt>>,
    notices: RefCell<Vec<RunningControl>>,
    while_deciding: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Approver for FakeApprover {
    fn approve(
        &self,
        prompt: GrantPrompt,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Approval> + 'static>> {
        let command = !matches!(prompt.request, PromptRequest::RuleChange { .. });
        self.prompts.borrow_mut().push(prompt);
        if let Some(action) = self.while_deciding.borrow_mut().take() {
            action();
        }
        let mut approval = Approval::from(self.decision.clone());
        if approval.decision == Decision::Allow && command {
            let (running, control) = Running::new();
            approval.running = Some(running);
            self.notices.borrow_mut().push(control);
        }
        Box::pin(async move { approval })
    }
}

/// The grant that the guest gets.
struct GrantReply {
    proxy: network_proxy::Client,
    hosts: Vec<String>,
    duration_secs: u32,
}

/// A deny that the guest gets: `(by_user, reason)`.
type Deny = (bool, String);

/// Build the network of a project with `toml` as its `airlock.toml`. The
/// grants go to an approver with the decision `decision`.
/// Returns:
///   The network, the approver to check its prompts, and the project dir.
fn grant_network(toml: &str, decision: Decision) -> (Network, Rc<FakeApprover>, TempDir) {
    let approver = Rc::new(FakeApprover {
        decision,
        prompts: RefCell::default(),
        notices: RefCell::default(),
        while_deciding: RefCell::default(),
    });
    // The test upstreams listen on loopback, which a real grant refuses.
    crate::network::grant::allow_loopback_on_this_thread();
    let dir = temp_dir();
    // The project file has the config. With no config of the last run, the
    // grant context takes the approved rules from it.
    std::fs::write(dir.path().join("airlock.toml"), toml).unwrap();
    let network = network_in_project(toml, dir.path())
        .unwrap()
        .network
        .with_grants(GrantContext::new(
            "test-sandbox".into(),
            approver.clone(),
            VerifiedPath::new_follow(dir.path(), None).unwrap(),
            VerifiedPath::new_follow(dir.path().join("sandbox"), None).unwrap(),
        ));
    (network, approver, dir)
}

/// Ask `proxy` to run a command with `hosts` or with the on-demand rule
/// `rule`.
/// Returns:
///   The grant, or the deny.
async fn request(
    proxy: &network_proxy::Client,
    hosts: &[&str],
    rule: &str,
) -> Result<GrantReply, Deny> {
    let mut req = proxy.request_grant_request();
    let mut r = req.get().init_req();
    r.set_cmd("curl https://example.com");
    r.set_cwd("/work");
    r.set_rule(rule);
    let mut list = r.init_hosts(hosts.len() as u32);
    for (i, h) in hosts.iter().enumerate() {
        list.set(i as u32, *h);
    }
    let resp = req.send().promise.await.unwrap();
    match resp.get().unwrap().get_result().unwrap().which().unwrap() {
        grant_result::Granted(g) => {
            let g = g.unwrap();
            Ok(GrantReply {
                proxy: g.get_proxy().unwrap(),
                hosts: texts(g.get_hosts().unwrap()),
                duration_secs: g.get_duration_secs(),
            })
        }
        grant_result::Denied(d) => Err(deny(d.unwrap())),
    }
}

/// Ask `proxy` for a grant of `hosts`.
async fn request_grant(proxy: &network_proxy::Client, hosts: &[&str]) -> Result<GrantReply, Deny> {
    request(proxy, hosts, "").await
}

fn texts(l: capnp::text_list::Reader<'_>) -> Vec<String> {
    l.iter()
        .map(|t| t.unwrap().to_str().unwrap().to_string())
        .collect()
}

fn deny(d: airlock_common::network_capnp::denied::Reader<'_>) -> Deny {
    (
        d.get_by_user(),
        d.get_reason().unwrap().to_str().unwrap().to_string(),
    )
}

/// Get the connection log of a grant proxy as `(host, port, blocked)`.
async fn grant_log(proxy: &network_proxy::Client) -> Vec<(String, u16, bool)> {
    let resp = proxy.grant_log_request().send().promise.await.unwrap();
    let entries = resp.get().unwrap().get_entries().unwrap();
    entries
        .iter()
        .map(|e| {
            (
                e.get_host().unwrap().to_str().unwrap().to_string(),
                e.get_port(),
                e.get_blocked(),
            )
        })
        .collect()
}

/// One rule of `listRules`.
#[derive(Debug, PartialEq, Eq)]
struct RuleRow {
    name: String,
    enabled: String,
    description: String,
    allow: Vec<String>,
    deny: Vec<String>,
    passthrough: bool,
    inject: bool,
}

/// Get the rules that the sandbox can see.
async fn list_rules(proxy: &network_proxy::Client) -> Vec<RuleRow> {
    let resp = proxy.list_rules_request().send().promise.await.unwrap();
    let rules = resp.get().unwrap().get_rules().unwrap();
    let text = |t: capnp::text::Reader<'_>| t.to_str().unwrap().to_string();
    rules
        .iter()
        .map(|r| RuleRow {
            name: text(r.get_name().unwrap()),
            enabled: text(r.get_enabled().unwrap()),
            description: text(r.get_description().unwrap()),
            allow: texts(r.get_allow().unwrap()),
            deny: texts(r.get_deny().unwrap()),
            passthrough: r.get_passthrough(),
            inject: r.get_inject(),
        })
        .collect()
}

/// Get the names of the rules that the sandbox can see.
async fn rule_names(proxy: &network_proxy::Client) -> Vec<String> {
    list_rules(proxy)
        .await
        .into_iter()
        .map(|r| r.name)
        .collect()
}

/// Result of a rule change.
#[derive(Debug, PartialEq, Eq)]
enum Upsert {
    Approved,
    Denied(Deny),
    Error(String),
}

/// A rule change. `None` values do not change.
#[derive(Default)]
struct Change<'a> {
    name: &'a str,
    enabled: &'a str,
    description: Option<&'a str>,
    allow: Option<&'a [&'a str]>,
    deny: Option<&'a [&'a str]>,
}

/// Ask `proxy` to add or change a rule with a description, allow and deny
/// patterns.
async fn upsert(
    proxy: &network_proxy::Client,
    name: &str,
    allow: &[&str],
    deny_list: &[&str],
) -> Upsert {
    let change = Change {
        name,
        description: Some("test rule"),
        allow: Some(allow),
        deny: Some(deny_list),
        ..Change::default()
    };
    upsert_change(proxy, &change).await
}

/// Ask `proxy` to make a rule change.
async fn upsert_change(proxy: &network_proxy::Client, change: &Change<'_>) -> Upsert {
    let mut req = proxy.upsert_rule_request();
    let mut r = req.get().init_rule();
    r.set_name(change.name);
    r.set_enabled(change.enabled);
    if let Some(d) = change.description {
        r.set_description(d);
        r.set_set_description(true);
    }
    for (key, given) in [("allow", change.allow), ("deny", change.deny)] {
        let Some(given) = given else { continue };
        let mut list = if key == "allow" {
            r.set_set_allow(true);
            r.reborrow().init_allow(given.len() as u32)
        } else {
            r.set_set_deny(true);
            r.reborrow().init_deny(given.len() as u32)
        };
        for (i, h) in given.iter().enumerate() {
            list.set(i as u32, *h);
        }
    }
    let resp = req.send().promise.await.unwrap();
    match resp.get().unwrap().get_result().unwrap().which().unwrap() {
        upsert_result::Approved(()) => Upsert::Approved,
        upsert_result::Denied(d) => Upsert::Denied(deny(d.unwrap())),
        upsert_result::Error(e) => Upsert::Error(e.unwrap().to_str().unwrap().to_string()),
    }
}

/// Send a GET to `127.0.0.1:port` through `proxy`.
/// Returns:
///   The response text.
async fn get(proxy: &network_proxy::Client, port: u16) -> String {
    TestConnection::local(proxy, port)
        .await
        .roundtrip(&http_get(port, "/"))
        .await
}

/// Start a TCP echo server on loopback for one connection.
/// Returns:
///   The port, and a receiver that fires when the server reads EOF from
///   the proxy.
async fn echo_with_eof() -> (u16, oneshot::Receiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (eof_tx, eof_rx) = oneshot::channel();
    tokio::task::spawn_local(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        while let Ok(n) = sock.read(&mut buf).await {
            if n == 0 || sock.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = eof_tx.send(());
    });
    (port, eof_rx)
}

/// Start an HTTP server on loopback for one request. It answers with a
/// chunked body that never ends.
/// Returns:
///   The port, and a receiver that fires when a write to the proxy fails
///   (the proxy closed the connection).
async fn endless_stream() -> (u16, oneshot::Receiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (closed_tx, closed_rx) = oneshot::channel();
    tokio::task::spawn_local(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "proxy closed before the request ended");
            req.extend_from_slice(&buf[..n]);
        }
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut out = sock.write_all(head).await;
        while out.is_ok() {
            out = sock.write_all(b"5\r\nhello\r\n").await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = closed_tx.send(());
    });
    (port, closed_rx)
}

/// Config with deny by default, grants on and editing of on-demand rules.
const OPTED_IN: &str = r#"
[network]
policy = "deny-by-default"
rule_editing = "on-demand"

[network.grants]
enabled = true
"#;

/// Test that a granted host is allowed only through the grant proxy. The
/// default proxy must still deny it, and the grant proxy must deny hosts
/// outside the grant. The grant carries the base env and cwd of the
/// sandbox, so the guest runs the command as `airlock exec` does.
///   1. Ask for a grant of one upstream and approve it
///   2. Check the prompt and the grant result (hosts, duration from the
///      config, env, cwd)
///   3. Send a GET through the grant proxy and check that it gets through
///   4. Send the same GET through the default proxy and check 403
///   5. Send a GET to a second upstream through the grant proxy and check
///      403
///   6. Check the connection log of the grant: one allowed and one blocked
///      target, with no duplicates
///   7. Check that the grant proxy refuses a new grant request, and the
///      default proxy refuses the log request
#[test]
fn granted_host_is_allowed_only_through_grant_proxy() {
    let granted = AcceptCounter::bind();
    let other = AcceptCounter::bind();
    let (granted_port, other_port) = (granted.port(), other.port());
    let (network, approver, _dir) = grant_network(OPTED_IN, Decision::Allow);
    run_network(network, |proxy| async move {
        let granted_hits = granted.start();
        let other_hits = other.start();
        let host = format!("127.0.0.1:{granted_port}");
        let grant = request_grant(&proxy, &[&host, &host]).await.unwrap();

        let prompts = approver.prompts.borrow().clone();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].sandbox, "test-sandbox");
        assert_eq!(
            prompts[0].request,
            PromptRequest::Hosts {
                hosts: vec![host.clone()],
                command: test_command(),
            }
        );
        assert_eq!(grant.hosts, [host]);
        assert_eq!(grant.duration_secs, 900);

        let resp = get(&grant.proxy, granted_port).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        let resp = get(&proxy, granted_port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        let resp = get(&grant.proxy, other_port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        let resp = get(&grant.proxy, granted_port).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert_eq!(granted_hits.load(Ordering::SeqCst), 2);
        assert_eq!(other_hits.load(Ordering::SeqCst), 0);
        assert_eq!(
            grant_log(&grant.proxy).await,
            [
                ("127.0.0.1".to_string(), granted_port, false),
                ("127.0.0.1".to_string(), other_port, true),
            ]
        );

        let nested = request_grant(&grant.proxy, &["example.com"]).await;
        assert!(nested.is_err());
        assert!(proxy.grant_log_request().send().promise.await.is_err());
    });
}

/// Test that a config deny rule wins over a grant. A grant must not open
/// a host that the user denied in the config.
///   1. Deny the upstream with a config rule
///   2. Ask for a grant of the upstream and approve it
///   3. Send a GET through the grant proxy and check 403
#[test]
fn config_deny_rule_wins_over_grant() {
    let upstream = AcceptCounter::bind();
    let port = upstream.port();
    let toml = format!(
        r#"
        [network]
        policy = "deny-by-default"

        [network.grants]
        enabled = true

        [network.rules.block]
        deny = ["127.0.0.1:{port}"]
        "#
    );
    let (network, _, _dir) = grant_network(&toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let hits = upstream.start();
        let grant = request_grant(&proxy, &[&format!("127.0.0.1:{port}")])
            .await
            .unwrap();
        let resp = get(&grant.proxy, port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    });
}

/// Test that requests for too wide, local or numeric hosts are denied
/// before the user sees them. The user must never get a prompt that opens
/// a public suffix or the host machine. Only `*` alone asks for all hosts. The system resolver
/// reads numeric names such as `2130706433` as loopback addresses.
///   1. Ask for grants of wildcards, public suffixes, local names, private
///      addresses, numeric address forms, bad labels and an empty list
///   2. Check that each request is denied
///   3. Check that the approver got no prompt
#[test]
fn wide_local_and_numeric_grant_hosts_are_refused_before_approval() {
    let (network, approver, _dir) = grant_network(OPTED_IN, Decision::Allow);
    let long_label = format!("{}.com", "a".repeat(64));
    let long_name = format!("{}com", "abcdefghi.".repeat(26));
    run_network(network, |proxy| async move {
        for hosts in [
            &["example.com", "*.com"][..],
            &["*.co.uk"],
            &["*.github.io"],
            &["co.uk"],
            &["a.*.example.com"],
            &["localhost:8080"],
            &["api.localhost"],
            &["intranet"],
            &["metadata.google.internal"],
            &["printer.local"],
            &["nas.lan"],
            &["router.home.arpa"],
            &["admin.airlock"],
            &["192.168.1.10"],
            &["169.254.169.254"],
            &["[fd00::1]:443"],
            &["2130706433"],
            &["0x7f.1"],
            &["0x7f000001.example.com"],
            &["017700000001"],
            &["127.1"],
            &["127.0x1"],
            &["1.0xa8.0x1"],
            &["0"],
            &["0.0.0.0"],
            &["-bad.example.com"],
            &["bad-.example.com"],
            &[long_label.as_str()],
            &[long_name.as_str()],
            &["bad host.com"],
            &["example.com:0"],
            &["example.com:https"],
            &[],
        ] {
            let result = request_grant(&proxy, hosts).await;
            assert!(result.is_err(), "{hosts:?}");
        }
        assert!(approver.prompts.borrow().is_empty());
    });
}

/// Test that a request is denied with no prompt when the network already
/// allows all its hosts. A prompt for that would only train the user to
/// click allow.
///   1. Allow one host with a config rule
///   2. Ask for a grant of that host and check the deny text
///   3. Ask for that host and one more host, and check that the user gets
///      a prompt
#[test]
fn grant_of_already_allowed_hosts_is_denied_without_prompt() {
    let toml = r#"
        [network]
        policy = "deny-by-default"

        [network.grants]
        enabled = true

        [network.rules.api]
        allow = ["api.example.com"]
        "#;
    let (network, approver, _dir) = grant_network(toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let (_, err) = request_grant(&proxy, &["api.example.com:443"])
            .await
            .err()
            .unwrap();
        assert!(err.contains("already allows"), "{err}");
        assert!(approver.prompts.borrow().is_empty());

        request_grant(&proxy, &["api.example.com", "docs.example.com"])
            .await
            .unwrap();
        assert_eq!(approver.prompts.borrow().len(), 1);
    });
}

/// Test that the guest gets the deny text when the user denies, and when
/// the config disables grants. After a user deny, new requests are denied
/// for a short time with no prompt, so a loop in the agent cannot spam the
/// user.
///   1. Ask for a grant from an approver that denies as the user
///   2. Check the deny text and that it is a user deny
///   3. Ask again and check the cooldown text and that no prompt came. A
///      rule change is also in the cooldown.
///   4. Ask for a grant on a network with `network.grants.enabled = false`
///   5. Check that it is denied with no prompt
///   6. Check that the rules still show and that a command with an
///      on-demand rule still goes to the user. Rule editing does not need
///      grants.
#[test]
fn grant_denied_by_user_or_config_returns_deny_text() {
    let (network, approver, _dir) = grant_network(OPTED_IN, Decision::DeniedByUser);
    run_network(network, |proxy| async move {
        let err = request_grant(&proxy, &["example.com"]).await.err().unwrap();
        assert_eq!(
            err,
            (true, crate::messages::grants::DENIED_BY_USER.to_string())
        );
        let (by_user, err) = request_grant(&proxy, &["example.com"]).await.err().unwrap();
        assert!(!by_user && err.contains("30 seconds"), "{err}");
        let Upsert::Denied((false, err)) = upsert(&proxy, "r", &["example.com"], &[]).await else {
            panic!("rule change is not in the cooldown");
        };
        assert!(err.contains("30 seconds"), "{err}");
        assert_eq!(approver.prompts.borrow().len(), 1);
    });

    let toml = r#"
        [network]
        rule_editing = "on-demand"

        [network.grants]
        enabled = false

        [network.rules.local]
        enabled = "on-demand"
        allow = ["127.0.0.1"]
        "#;
    let (network, approver, _dir) = grant_network(toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let (by_user, err) = request_grant(&proxy, &["example.com"]).await.err().unwrap();
        assert!(!by_user && err.contains("disabled"), "{err}");
        assert!(approver.prompts.borrow().is_empty());

        assert_eq!(rule_names(&proxy).await, ["local"]);
        let grant = request(&proxy, &[], "local").await.unwrap();
        assert_eq!(grant.hosts, ["127.0.0.1"]);
        assert_eq!(approver.prompts.borrow().len(), 1);
    });
}

/// Test that a zero grant time in the config is an error. A zero approval
/// timeout would deny all requests, and a zero duration would end each
/// grant at once.
///   1. Resolve configs with `approval_timeout = 0` and `max_duration = 0`
///   2. Check that each config error names the field
#[test]
fn zero_grant_time_in_config_is_an_error() {
    for field in ["approval_timeout", "max_duration"] {
        let toml = format!("[network.grants]\n{field} = 0\n");
        let err = crate::test_cfg::config::project_toml_error(&toml);
        assert!(err.contains(&format!("network.grants.{field}")), "{err}");
    }
}

/// Test that a release of the grant proxy closes the open connections of
/// the grant on both sides. Thus no access stays after the command ends.
///   1. Open a raw TCP connection to an echo upstream through the grant
///      proxy and check the echo
///   2. Release the grant proxy
///   3. Check that the guest side closes and the upstream reads EOF
#[test]
fn released_grant_closes_live_connection() {
    let (network, _, _dir) = grant_network(OPTED_IN, Decision::Allow);
    run_network(network, |proxy| async move {
        let (port, eof) = echo_with_eof().await;
        let grant = request_grant(&proxy, &[&format!("127.0.0.1:{port}")])
            .await
            .unwrap();
        let mut conn = TestConnection::local(&grant.proxy, port).await;
        conn.send(b"PING\r\n").await;
        conn.recv_until(|t| t.contains("PING")).await;

        drop(grant);
        conn.recv_until_closed(3000).await;
        tokio::time::timeout(Duration::from_secs(3), eof)
            .await
            .expect("upstream reads EOF")
            .unwrap();
    });
}

/// Test that a grant ends at its duration, limited by the config maximum.
/// The open connections close, and new connections are denied.
///   1. Set `max_duration = 1` and ask for 60 seconds
///   2. Check that the grant has 1 second
///   3. Open a connection to an echo upstream through the grant proxy
///   4. Check that the connection closes after the duration
///   5. Check that a new connection through the grant proxy is denied
#[test]
fn expired_grant_closes_connection_and_refuses_new_ones() {
    let toml = "[network]\npolicy = \"deny-by-default\"\n[network.grants]\nenabled = true\nmax_duration = 1\n";
    let (network, _, _dir) = grant_network(toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let (port, eof) = echo_with_eof().await;
        let grant = request_grant(&proxy, &[&format!("127.0.0.1:{port}")])
            .await
            .unwrap();
        assert_eq!(grant.duration_secs, 1);
        let mut conn = TestConnection::local(&grant.proxy, port).await;
        conn.send(b"PING\r\n").await;
        conn.recv_until(|t| t.contains("PING")).await;

        conn.recv_until_closed(4000).await;
        tokio::time::timeout(Duration::from_secs(3), eof)
            .await
            .expect("upstream reads EOF")
            .unwrap();
        assert!(
            TestConnection::connect(&grant.proxy, "127.0.0.1", port)
                .await
                .is_none()
        );
    });
}

/// Test that only a deny of the default policy is grantable. The guest
/// offers grantable denies to the agent, so a deny rule or a local host
/// must not show as grantable.
///   1. Build a deny-by-default network with an allow and a deny rule
///   2. Resolve allowed, rule-denied, local and unknown public targets
///   3. Check the grantable flag of each
#[test]
fn only_default_policy_deny_of_public_host_is_grantable() {
    let network = network_from_toml(
        r#"
        [network]
        policy = "deny-by-default"

        [network.rules.r]
        allow = ["allowed.example.com"]
        deny = ["blocked.example.com"]
        "#,
    )
    .unwrap()
    .network;
    for (host, grantable) in [
        ("allowed.example.com", false),
        ("blocked.example.com", false),
        ("localhost", false),
        ("10.0.0.5", false),
        ("unknown.example.com", true),
        ("8.8.8.8", true),
    ] {
        assert_eq!(
            network.resolve_target(host, 443).grantable,
            grantable,
            "{host}"
        );
    }
}

/// Test that a release of the grant proxy closes an HTTP connection that
/// still streams a response. The HTTP stack runs helper tasks for the
/// upstream connection. They must stop too, or the upstream stays open.
///   1. Send a GET through the grant proxy to a server that streams a
///      body with no end
///   2. Read the first chunk
///   3. Release the grant proxy
///   4. Check that the guest side closes and the upstream write fails
#[test]
fn released_grant_closes_streaming_http_connection() {
    let (network, _, _dir) = grant_network(OPTED_IN, Decision::Allow);
    run_network(network, |proxy| async move {
        let (port, closed) = endless_stream().await;
        let grant = request_grant(&proxy, &[&format!("127.0.0.1:{port}")])
            .await
            .unwrap();
        let mut conn = TestConnection::local(&grant.proxy, port).await;
        conn.send(http_get(port, "/").as_bytes()).await;
        conn.recv_until(|t| t.contains("hello")).await;

        drop(grant);
        conn.recv_until_closed(3000).await;
        tokio::time::timeout(Duration::from_secs(3), closed)
            .await
            .expect("upstream connection closes")
            .unwrap();
    });
}

/// Test that a connection that only a grant allows goes to public
/// addresses only. A granted name can resolve to a local address (DNS
/// rebinding, cloud metadata names). Hosts that the config allows keep
/// the normal dial.
///   1. Build a deny-by-default network with one allow rule
///   2. Resolve a granted host and the allowed host with a grant
///   3. Check that only the granted host is public-only
#[test]
fn connection_allowed_only_by_grant_dials_public_addresses_only() {
    let network = network_from_toml(
        r#"
        [network]
        policy = "deny-by-default"

        [network.rules.r]
        allow = ["allowed.example.com"]
        "#,
    )
    .unwrap()
    .network;
    let grant = GrantAccess::Hosts(vec![
        NetworkTarget {
            host: "granted.example.com".into(),
            port: None,
        },
        NetworkTarget {
            host: "allowed.example.com".into(),
            port: None,
        },
    ]);
    let granted = network.resolve_target_in("granted.example.com", 443, Some(&grant));
    assert!(granted.allowed && granted.public_only);
    let allowed = network.resolve_target_in("allowed.example.com", 443, Some(&grant));
    assert!(allowed.allowed && !allowed.public_only);
}

/// Test that an on-demand rule is not active for normal connections, and
/// that an approved command with the rule gets its allow and deny
/// patterns. The sandbox must not use the rule with no approval.
///   1. Configure an on-demand rule that allows loopback and denies one
///      port
///   2. Send a GET through the default proxy and check 403
///   3. Ask for an unknown rule, and for hosts and a rule together, and
///      check that both are denied with no prompt
///   4. Ask for the rule and check the prompt and the hosts of the grant
///   5. Send GETs through the grant proxy: the allowed port gets through,
///      the denied port gets 403
#[test]
fn on_demand_rule_is_inactive_until_command_with_rule_is_approved() {
    let allowed = AcceptCounter::bind();
    let denied = AcceptCounter::bind();
    let (allowed_port, denied_port) = (allowed.port(), denied.port());
    let toml = format!(
        r#"
        [network]
        policy = "deny-by-default"
        rule_editing = "on-demand"

        [network.rules.local]
        enabled = "on-demand"
        description = "local test servers"
        allow = ["127.0.0.1"]
        deny = ["127.0.0.1:{denied_port}"]
        "#
    );
    let (network, approver, _dir) = grant_network(&toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let allowed_hits = allowed.start();
        let denied_hits = denied.start();
        let resp = get(&proxy, allowed_port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");

        let (by_user, err) = request(&proxy, &[], "nope").await.err().unwrap();
        assert!(!by_user && err.contains("nope"), "{err}");
        assert!(request(&proxy, &["example.com"], "local").await.is_err());
        assert!(request(&proxy, &[], "").await.is_err());
        assert!(approver.prompts.borrow().is_empty());

        let grant = request(&proxy, &[], "local").await.unwrap();
        let prompts = approver.prompts.borrow().clone();
        let [
            GrantPrompt {
                request:
                    PromptRequest::Rule {
                        name,
                        rule,
                        command,
                    },
                ..
            },
        ] = &prompts[..]
        else {
            panic!("{prompts:?}");
        };
        assert_eq!((name.as_str(), command), ("local", &test_command()));
        assert_eq!(rule.description, "local test servers");
        assert_eq!(grant.hosts, ["127.0.0.1"]);
        let resp = get(&grant.proxy, allowed_port).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        let resp = get(&grant.proxy, denied_port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        assert_eq!(allowed_hits.load(Ordering::SeqCst), 1);
        assert_eq!(denied_hits.load(Ordering::SeqCst), 0);
    });
}

/// Command of the test requests, as the prompt shows it.
fn test_command() -> PromptCommand {
    PromptCommand {
        cmd: "curl https://example.com".into(),
        cwd: "/work".into(),
        env: vec![],
        duration_secs: 900,
    }
}

/// Test that an approved change of an on-demand rule applies at once, goes
/// into the project config file, and stays inactive for normal
/// connections. Bad changes are refused with no prompt.
///   1. Configure a normal rule and an on-demand rule, and list the rules
///   2. Change the normal rule, use a bad name and a local host, and check
///      that each is refused with no prompt
///   3. Add a new rule and check the prompt, the list and the config file
///   4. Check that the default proxy still denies the host of the rule, and
///      that a command with the rule gets through
///   5. Deny a change as the user and check the result
#[test]
fn approved_rule_change_is_saved_and_usable_only_through_command() {
    let upstream = AcceptCounter::bind();
    let port = upstream.port();
    let toml = r#"
        [network]
        policy = "deny-by-default"
        rule_editing = "on-demand"

        [network.rules.normal]
        allow = ["normal.example.com"]

        [network.rules.docs]
        enabled = "on-demand"
        description = "docs"
        allow = ["docs.example.com"]
        "#;
    let (network, approver, dir) = grant_network(toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let hits = upstream.start();
        assert_eq!(
            list_rules(&proxy).await,
            [RuleRow {
                name: "docs".into(),
                enabled: "on-demand".into(),
                description: "docs".into(),
                allow: vec!["docs.example.com".into()],
                deny: vec![],
                passthrough: false,
                inject: false,
            }]
        );

        assert_eq!(
            upsert(&proxy, "normal", &["a.example.com"], &[]).await,
            Upsert::Denied((false, msg::rule_name_taken("normal")))
        );
        for (name, allow) in [("Bad Name", "a.example.com"), ("ok", "localhost")] {
            let result = upsert(&proxy, name, &[allow], &[]).await;
            assert!(matches!(result, Upsert::Error(_)), "{name}: {result:?}");
        }
        assert!(approver.prompts.borrow().is_empty());

        let host = format!("127.0.0.1:{port}");
        assert_eq!(
            upsert(&proxy, "local", &[&host], &["evil.example.com"]).await,
            Upsert::Approved
        );
        let prompt = approver.prompts.borrow()[0].request.clone();
        let PromptRequest::RuleChange {
            name, rule, old, ..
        } = &prompt
        else {
            panic!("not a rule change");
        };
        assert_eq!((name.as_str(), old), ("local", &None));
        assert_eq!(
            (rule.enabled, rule.description.as_str()),
            (RuleEnabled::OnDemand, "test rule")
        );
        assert_eq!(
            (&rule.allow, &rule.deny),
            (&vec![host.clone()], &vec!["evil.example.com".into()])
        );
        assert_eq!(rule_names(&proxy).await, ["docs", "local"]);
        let saved = std::fs::read_to_string(dir.path().join("airlock.toml")).unwrap();
        assert!(
            saved.contains("[network.rules.local]\nenabled = \"on-demand\""),
            "{saved}"
        );

        let resp = get(&proxy, port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        let grant = request(&proxy, &[], "local").await.unwrap();
        let resp = get(&grant.proxy, port).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    });

    let (network, _, _dir) = grant_network(OPTED_IN, Decision::DeniedByUser);
    run_network(network, |proxy| async move {
        assert_eq!(
            upsert(&proxy, "r", &["example.com"], &[]).await,
            Upsert::Denied((true, msg::DENIED_BY_USER.into()))
        );
        assert!(list_rules(&proxy).await.is_empty());
    });
}

/// Test that with `rule_editing = "all"` a change of a normal rule sets
/// only the given values, keeps its other keys in the file and live, and
/// applies at once to new connections. A rule can also move between the
/// normal and the on-demand rules.
///   1. Configure a normal rule with `passthrough` and a comment, and an
///      on-demand rule
///   2. Check that the list shows both rules with their flags
///   3. Add the upstream to the allow list of the normal rule, and check
///      the file: the new list, the kept comment and `passthrough`
///   4. Check that a new connection gets through with passthrough
///   5. Turn the normal rule to on demand and check that the default
///      proxy denies the upstream, and a command with the rule allows it
#[test]
fn normal_rule_change_keeps_other_keys_and_applies_live() {
    let upstream = AcceptCounter::bind();
    let port = upstream.port();
    let toml = r#"
[network]
policy = "deny-by-default"
rule_editing = "all"

# Raw TCP.
[network.rules.db]
allow = ["db.example.com"]  # the db
passthrough = true

[network.rules.docs]
enabled = "on-demand"
allow = ["docs.example.com"]
"#;
    let (network, approver, dir) = grant_network(toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let hits = upstream.start();
        let rules = list_rules(&proxy).await;
        assert_eq!(
            rules
                .iter()
                .map(|r| (r.name.as_str(), r.enabled.as_str(), r.passthrough))
                .collect::<Vec<_>>(),
            [("db", "true", true), ("docs", "on-demand", false)]
        );
        let resp = get(&proxy, port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");

        let host = format!("127.0.0.1:{port}");
        let allow = ["db.example.com", host.as_str()];
        let change = Change {
            name: "db",
            allow: Some(&allow),
            ..Change::default()
        };
        assert_eq!(upsert_change(&proxy, &change).await, Upsert::Approved);
        let saved = std::fs::read_to_string(dir.path().join("airlock.toml")).unwrap();
        assert!(
            saved.contains(&format!(
                "# Raw TCP.\n[network.rules.db]\nallow = [\"db.example.com\", \"{host}\"]  \
                 # the db\npassthrough = true\n\n"
            )),
            "{saved}"
        );
        let rules = list_rules(&proxy).await;
        assert!(
            rules
                .iter()
                .any(|r| r.name == "db" && r.allow.contains(&host))
        );
        let mut conn = TestConnection::local(&proxy, port).await;
        conn.send(b"raw bytes\r\n\r\n").await;
        conn.recv_until(|t| t.contains("204")).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let change = Change {
            name: "db",
            enabled: "on-demand",
            ..Change::default()
        };
        assert_eq!(upsert_change(&proxy, &change).await, Upsert::Approved);
        let resp = get(&proxy, port).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
        let grant = request(&proxy, &[], "db").await.unwrap();
        let mut conn = TestConnection::local(&grant.proxy, port).await;
        conn.send(b"raw bytes\r\n\r\n").await;
        conn.recv_until(|t| t.contains("204")).await;
        assert_eq!(approver.prompts.borrow().len(), 3);
    });
}

/// Test that `rule_editing` limits the rules that the sandbox sees and
/// changes. A refused change is denied at once with no prompt.
///   1. With "on-demand": check that only the on-demand rule shows, and
///      that a change of the normal rule and a change to `enabled = true`
///      are denied
///   2. With "none": check that no rule shows, that each change is denied,
///      and that a command with the rule is denied
///   3. Check that the user saw no prompt
#[test]
fn rule_editing_limits_listed_and_changed_rules() {
    let toml = |editing: &str| {
        format!(
            r#"
            [network]
            policy = "deny-by-default"
            rule_editing = "{editing}"

            [network.rules.normal]
            allow = ["normal.example.com"]

            [network.rules.docs]
            enabled = "on-demand"
            allow = ["docs.example.com"]
            "#
        )
    };
    let (network, approver, _dir) = grant_network(&toml("on-demand"), Decision::Allow);
    run_network(network, |proxy| async move {
        assert_eq!(rule_names(&proxy).await, ["docs"]);
        assert_eq!(
            upsert(&proxy, "normal", &["a.example.com"], &[]).await,
            Upsert::Denied((false, msg::rule_name_taken("normal")))
        );
        let change = Change {
            name: "docs",
            enabled: "true",
            ..Change::default()
        };
        assert_eq!(
            upsert_change(&proxy, &change).await,
            Upsert::Denied((false, msg::ONLY_ON_DEMAND_EDITABLE.into()))
        );
        assert!(approver.prompts.borrow().is_empty());
    });

    let (network, approver, _dir) = grant_network(&toml("none"), Decision::Allow);
    run_network(network, |proxy| async move {
        assert!(list_rules(&proxy).await.is_empty());
        for name in ["docs", "new"] {
            assert_eq!(
                upsert(&proxy, name, &["a.example.com"], &[]).await,
                Upsert::Denied((false, msg::RULE_EDITING_DISABLED.into()))
            );
        }
        let (by_user, err) = request(&proxy, &[], "docs").await.err().unwrap();
        assert!(!by_user && err.contains("disabled"), "{err}");
        assert!(approver.prompts.borrow().is_empty());
    });
}

/// Test that a request for all hosts must have only `*`, and that it
/// allows public names. Deny rules, service hosts and local names still
/// win, and the connections go to public addresses only.
///   1. Ask for `*` with another host and check the deny with no prompt
///   2. Ask for `*` alone and check the prompt and the grant hosts
///   3. Resolve hosts with the grant access: a public name is allowed and
///      public-only, a denied name, a local name and a local IP are denied
#[test]
fn all_hosts_grant_must_be_alone_and_keeps_deny_rules_and_public_only() {
    let toml = r#"
        [network]
        policy = "deny-by-default"

        [network.grants]
        enabled = true

        [network.rules.block]
        deny = ["blocked.example.com"]
        "#;
    let (network, approver, _dir) = grant_network(toml, Decision::Allow);
    // The grant resolves without the loopback exception of the test.
    crate::network::grant::deny_loopback_on_this_thread();
    {
        let access = GrantAccess::AllHosts;
        let t = network.resolve_target_in("any.example.org", 443, Some(&access));
        assert!(t.allowed && t.public_only);
        for host in [
            "blocked.example.com",
            "localhost",
            "printer.local",
            "10.0.0.1",
            "127.0.0.1",
        ] {
            let t = network.resolve_target_in(host, 443, Some(&access));
            assert!(!t.allowed, "{host}");
        }
    }
    run_network(network, |proxy| async move {
        let (by_user, err) = request_grant(&proxy, &["*", "example.com"])
            .await
            .err()
            .unwrap();
        assert!(!by_user && err.contains('*'), "{err}");
        assert!(approver.prompts.borrow().is_empty());

        let grant = request_grant(&proxy, &["*"]).await.unwrap();
        assert_eq!(grant.hosts, ["*"]);
        assert_eq!(
            approver.prompts.borrow()[0].request,
            PromptRequest::Hosts {
                hosts: vec!["*".into()],
                command: test_command(),
            }
        );
    });
}

/// Test that an abort from the running notice ends the grant at once. The
/// live connections close, the guest learns about the abort, and the notice
/// goes away. A release with no abort also removes the notice.
///   1. Grant a command and open a raw connection to an echo upstream
///   2. Start a wait for the revoke of the grant
///   3. Select abort on the notice
///   4. Check that the connection closes, the wait gives `aborted`, the
///      notice closes and new connections are denied
///   5. Grant a second command, release it and check that its notice closes
#[test]
fn abort_from_running_notice_revokes_grant_and_closes_connection() {
    let (network, approver, _dir) = grant_network(OPTED_IN, Decision::Allow);
    run_network(network, |proxy| async move {
        let (port, eof) = echo_with_eof().await;
        let grant = request_grant(&proxy, &[&format!("127.0.0.1:{port}")])
            .await
            .unwrap();
        let mut conn = TestConnection::local(&grant.proxy, port).await;
        conn.send(b"PING\r\n").await;
        conn.recv_until(|t| t.contains("PING")).await;
        let revoked = grant.proxy.wait_revoked_request().send().promise;

        let RunningControl { abort, close } = approver.notices.borrow_mut().remove(0);
        abort.send(()).unwrap();
        conn.recv_until_closed(3000).await;
        tokio::time::timeout(Duration::from_secs(3), eof)
            .await
            .expect("upstream reads EOF")
            .unwrap();
        let revoked = tokio::time::timeout(Duration::from_secs(3), revoked)
            .await
            .expect("revoke wait resolves")
            .unwrap();
        assert!(revoked.get().unwrap().get_aborted());
        assert!(close.await.is_err(), "notice must close");
        assert!(
            TestConnection::connect(&grant.proxy, "127.0.0.1", port)
                .await
                .is_none()
        );

        let grant = request_grant(&proxy, &["example.com"]).await.unwrap();
        let RunningControl { close, .. } = approver.notices.borrow_mut().remove(0);
        drop(grant);
        tokio::time::timeout(Duration::from_secs(3), close)
            .await
            .expect("notice closes on release")
            .unwrap_err();
    });
}

/// Config with an on-demand rule `docs` that the sandbox can change.
const DOCS: &str = r#"[network]
policy = "deny-by-default"
rule_editing = "all"

[network.rules.docs]
enabled = "on-demand"
allow = ["docs.example.com"]
"#;

/// [`DOCS`] after the sandbox put an `inject` key and one more allow host
/// into the rule `docs`, and a new rule `evil` with `inject`.
const DOCS_PLANTED: &str = r#"[network]
policy = "deny-by-default"
rule_editing = "all"

[network.rules.docs]
enabled = "on-demand"
allow = ["docs.example.com", "attacker.example.com"]
inject = ["GITHUB_TOKEN"]

[network.rules.evil]
inject = ["GITHUB_TOKEN"]
"#;

/// Test that a rule change is refused with no prompt when the sandbox
/// changed the rule in the project file. The sandbox can write the file,
/// but the write keeps the other keys of the rule. Thus a planted key, for
/// example `inject` of a secret, must not go into the file with the
/// approval of the user.
///   1. Put `inject` and one more allow host into the rule `docs`, and a
///      new rule `evil` with `inject`, into the project file
///   2. Change only the description of `docs` and check the refusal
///   3. Add the rule `evil` and check the refusal
///   4. Check that the user saw no prompt, that the file did not change,
///      and that the live rule did not change
#[test]
fn rule_change_with_rule_planted_in_file_is_refused_without_prompt() {
    let (network, approver, dir) = grant_network(DOCS, Decision::Allow);
    let file = dir.path().join("airlock.toml");
    std::fs::write(&file, DOCS_PLANTED).unwrap();
    let path = file.display().to_string();
    run_network(network, |proxy| async move {
        let change = Change {
            name: "docs",
            description: Some("harmless"),
            ..Change::default()
        };
        assert_eq!(
            upsert_change(&proxy, &change).await,
            Upsert::Error(msg::rule_not_approved("docs", &path))
        );
        assert_eq!(
            upsert(&proxy, "evil", &["evil.example.com"], &[]).await,
            Upsert::Error(msg::rule_not_approved("evil", &path))
        );
        assert!(approver.prompts.borrow().is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), DOCS_PLANTED);
        let rules = list_rules(&proxy).await;
        assert_eq!(rules.len(), 1);
        assert_eq!(
            (rules[0].allow.as_slice(), rules[0].inject),
            (["docs.example.com".to_string()].as_slice(), false)
        );
    });
}

/// Test that a rule change is refused when the sandbox changes the rule in
/// the project file while the user decides. The user approved the rule as
/// the prompt showed it, not the planted keys.
///   1. Ask for a description change of `docs`. While the user decides,
///      put `inject` into the rule in the file
///   2. Approve the change and check the refusal
///   3. Check that the file has no approved write, and that the live rule
///      did not change
#[test]
fn rule_planted_in_file_while_user_decides_is_refused() {
    let (network, approver, dir) = grant_network(DOCS, Decision::Allow);
    let file = dir.path().join("airlock.toml");
    let path = file.display().to_string();
    let planted = file.clone();
    *approver.while_deciding.borrow_mut() = Some(Box::new(move || {
        std::fs::write(&planted, DOCS_PLANTED).unwrap();
    }));
    run_network(network, |proxy| async move {
        let change = Change {
            name: "docs",
            description: Some("harmless"),
            ..Change::default()
        };
        assert_eq!(
            upsert_change(&proxy, &change).await,
            Upsert::Error(msg::rule_not_approved("docs", &path))
        );
        assert_eq!(approver.prompts.borrow().len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), DOCS_PLANTED);
        let rules = list_rules(&proxy).await;
        assert_eq!(rules[0].description, "");
    });
}

/// Test that the prompt of a rule change gets the full rule table. The
/// user must see the secrets that the rule injects and the other keys of
/// the table, also when the change does not touch them.
///   1. Configure an on-demand rule with `inject` of a masked env entry and
///      a key that the rule does not use
///   2. Change its description and approve
///   3. Check that the prompt has `inject` in the old and the new rule, and
///      the other key
///   4. Check that the file and the live rule keep `inject`
#[test]
fn rule_change_prompt_shows_full_rule_table() {
    let toml = r#"[env]
GITHUB_TOKEN = { value = "secret-value", mask = true }

[network]
policy = "deny-by-default"
rule_editing = "all"

[network.rules.gh]
enabled = "on-demand"
allow = ["api.github.com"]
inject = ["GITHUB_TOKEN"]
note = "kept"
"#;
    let (network, approver, dir) = grant_network(toml, Decision::Allow);
    run_network(network, |proxy| async move {
        let change = Change {
            name: "gh",
            description: Some("GitHub API"),
            ..Change::default()
        };
        assert_eq!(upsert_change(&proxy, &change).await, Upsert::Approved);
        let prompt = approver.prompts.borrow()[0].request.clone();
        let PromptRequest::RuleChange {
            old: Some(old),
            rule,
            other,
            ..
        } = &prompt
        else {
            panic!("not a change of a rule: {prompt:?}");
        };
        let inject = vec!["GITHUB_TOKEN".to_string()];
        assert_eq!((&old.inject, &rule.inject), (&inject, &inject));
        assert_eq!(rule.description, "GitHub API");
        assert_eq!(other, &[("note".to_string(), "\"kept\"".to_string())]);
        let saved = std::fs::read_to_string(dir.path().join("airlock.toml")).unwrap();
        assert!(saved.contains("inject = [\"GITHUB_TOKEN\"]"), "{saved}");
        let rules = list_rules(&proxy).await;
        assert!(rules[0].inject && rules[0].description == "GitHub API");
    });
}
