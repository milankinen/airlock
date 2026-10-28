//! Tests for the host signals.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, signal_sender, test_config};
use crate::test_cfg::block_on_local;

/// Test that a signal during the agent start stops the agent and the
/// proxy at once. The user must not wait for the start time limit.
///   1. Use an agent that never replies to the handshake
///   2. Send `session/new` and wait for the agent start
///   3. Send `SIGTERM` and check that the proxy stops with code 143 before
///      the time limit
#[test]
fn signal_during_start_stops_proxy() {
    block_on_local(async {
        let signals = signal_sender();
        let starter = LocalStarter::new(&[("AGENT_NO_INIT", "1")]);
        let starts = starter.starts.clone();
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let msg = json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": params });
        client.send(&msg).await;
        while starts.get() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let started = Instant::now();
        signals.send(libc::SIGTERM).unwrap();
        assert_eq!(client.proxy_end().await, 128 + libc::SIGTERM);
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

/// Test that the agent gets the signals after the handshake. The agent
/// then decides what to do.
///   1. Start a session
///   2. Send `SIGUSR1`, which stops the fake agent
///   3. Check that the proxy stops with the exit code of the agent
#[test]
fn signal_after_handshake_goes_to_agent() {
    block_on_local(async {
        let signals = signal_sender();
        let starter = LocalStarter::new(&[]);
        let client = TestClient::session(starter, test_config(true), "/projects/a").await;
        signals.send(libc::SIGUSR1).unwrap();
        assert_eq!(client.proxy_end().await, 128 + libc::SIGUSR1);
    });
}

/// Test that a signal while the proxy waits for the agent exit kills the
/// agent at once. The user must not wait for the grace time.
///   1. Start a session with an agent that does not exit at the end of its
///      input, and a long grace time
///   2. Close the client input and send `SIGTERM`
///   3. Check that the proxy stops with the kill exit code before the
///      grace time ends
#[test]
fn signal_during_shutdown_kills_agent() {
    block_on_local(async {
        let signals = signal_sender();
        let starter = LocalStarter::new(&[("AGENT_EOF", "ignore")]);
        let mut config = test_config(true);
        config.shutdown_grace = Duration::from_secs(5);
        let mut client = TestClient::session(starter, config, "/projects/a").await;
        let started = Instant::now();
        client.close();
        // Let the proxy see the client end first. Else it sends the signal
        // to the agent.
        tokio::time::sleep(Duration::from_millis(200)).await;
        signals.send(libc::SIGTERM).unwrap();
        assert_eq!(client.proxy_end().await, 128 + libc::SIGKILL);
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}
