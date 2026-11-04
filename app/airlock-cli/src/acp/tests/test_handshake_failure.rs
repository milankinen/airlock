//! Tests for agents that do not complete the handshake.

use std::time::Duration;

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that the proxy stops an agent that does not complete the handshake
/// in time. The editor must get a reply, not wait without end.
///   1. Use an agent that never replies to the handshake, and a short time
///      limit
///   2. Send `session/new`, and a prompt to an unknown session before the
///      time ends
///   3. Check the unknown session error of the prompt and the time limit
///      error of the session
///   4. Check that a new session starts a new agent
#[test]
fn handshake_timeout_fails_session_and_handles_queued_requests() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_NO_INIT", "1")]);
        let launches = launcher.launches.clone();
        let mut config = test_config(true);
        config.handshake_timeout = Duration::from_millis(300);
        let mut client = TestClient::start(launcher, config);
        client.initialize(json!({})).await;

        let session = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let msg = |id: i64, method: &str, params: &Value| json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        client.send(&msg(2, "session/new", &session)).await;
        let prompt = json!({ "sessionId": "s", "prompt": [] });
        client.send(&msg(3, "session/prompt", &prompt)).await;

        let (response, _) = client.response(3).await;
        assert_eq!(response["error"]["code"], -32602);
        let (response, _) = client.response(2).await;
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("did not complete"), "{message}");

        client.send(&msg(4, "session/new", &session)).await;
        client.response(4).await;
        assert_eq!(launches.get(), 2);
    });
}

/// Test that the proxy refuses an agent with an other ACP version. The
/// proxy told the client that the agent uses version 1.
///   1. Use an agent that replies with version 2
///   2. Check the session error
#[test]
fn agent_with_other_protocol_version_fails_session() {
    block_on_local(async {
        let init = json!({ "protocolVersion": 2 }).to_string();
        let launcher = LocalProcessLauncher::new(&[("AGENT_INIT", &init)]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let (code, message) = client.session_error(2, "/projects/a").await;
        assert_eq!(code, -32603);
        assert!(message.contains("ACP version 2"), "{message}");
    });
}

/// Test that an agent that exits before the handshake fails the session.
///   1. Use an agent command that exits at once
///   2. Check the session error
#[test]
fn agent_exit_before_handshake_fails_session() {
    block_on_local(async {
        let mut config = test_config(true);
        config.argv = vec!["sh".into(), "-c".into(), "exit 3".into()];
        let mut client = TestClient::start(LocalProcessLauncher::new(&[]), config);
        client.initialize(json!({})).await;
        let (code, message) = client.session_error(2, "/projects/a").await;
        assert_eq!(code, -32603);
        assert!(message.contains("exited before"), "{message}");
    });
}

/// Test that the client end during the agent start stops the proxy at
/// once. An agent without a session has nothing to save.
///   1. Use an agent that never replies to the handshake
///   2. Send `session/new` and close the client input
///   3. Check that the proxy stops with code 0 before the time limit ends
#[test]
fn client_end_during_start_stops_proxy() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_NO_INIT", "1")]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let session = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let msg = json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": session });
        client.send(&msg).await;
        let started = std::time::Instant::now();
        assert_eq!(client.exit_code().await, 0);
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}
