//! Tests for the agent exit and the client end.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that the proxy stops with the exit code of the agent when the
/// agent exits, and that the last agent message without a newline gets to
/// the client. The editor sees the agent exit as the proxy exit.
///   1. Start a session
///   2. Make the agent write a message without a newline and exit with
///      code 7, and keep the client open
///   3. Check that the client gets the message
///   4. Check that the proxy stops with code 7
#[test]
fn agent_exit_code_is_proxy_exit_code() {
    block_on_local(async {
        let mut client =
            TestClient::session(LocalStarter::new(&[]), test_config(true), "/projects/a").await;
        client
            .send(&json!({ "jsonrpc": "2.0", "method": "test/exit" }))
            .await;
        while client.recv().await.pointer("/params/last").is_none() {}
        assert_eq!(client.exit_code().await, 7);
    });
}

/// Test that the end of the client input closes the agent input, and that
/// the proxy gives the exit code of the agent. The agent can then save its
/// state and exit. Its last message without a newline gets to the client.
///   1. Start a session with an agent that exits with code 5 at the end of
///      its input
///   2. Close the client input
///   3. Check that the client gets the last agent message
///   4. Check that the proxy stops with code 5
#[test]
fn client_end_closes_agent_input() {
    block_on_local(async {
        let mut client = TestClient::session(
            LocalStarter::new(&[("AGENT_EOF", "5")]),
            test_config(true),
            "/projects/a",
        )
        .await;
        client.close();
        while client.recv().await.pointer("/params/last").is_none() {}
        assert_eq!(client.proxy_end().await, 5);
    });
}

/// Test that the proxy kills an agent that does not exit after the client
/// end. Else the agent stays in the sandbox without a client.
///   1. Start a session with an agent that does not exit at the end of its
///      input
///   2. Close the client input
///   3. Check that the proxy stops after the grace time with the kill exit
///      code
#[test]
fn agent_that_ignores_client_end_is_killed() {
    block_on_local(async {
        let client = TestClient::session(
            LocalStarter::new(&[("AGENT_EOF", "ignore")]),
            test_config(true),
            "/projects/a",
        )
        .await;
        let started = Instant::now();
        assert_eq!(client.exit_code().await, 128 + libc::SIGKILL);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(500) && elapsed < Duration::from_secs(5),
            "{elapsed:?}"
        );
    });
}
