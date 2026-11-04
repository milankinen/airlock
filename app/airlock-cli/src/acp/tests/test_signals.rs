//! Tests for the host signals.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, signal_sender, test_config};
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
        let launcher = LocalProcessLauncher::new(&[("AGENT_NO_INIT", "1")]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let msg = json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": params });
        client.send(&msg).await;
        while launches.get() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let started = Instant::now();
        signals.send(libc::SIGTERM).unwrap();
        assert_eq!(client.proxy_end().await, 128 + libc::SIGTERM);
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

/// Test that a signal after the handshake stops all agents as the client
/// end does: their input closes, and they can save their state. The
/// proxy then stops with the agent exit code.
///   1. Start sessions in sandboxes `a` and `b` with agents that exit with
///      code 5 at the end of their input
///   2. Send `SIGTERM` and keep the client open
///   3. Check that the proxy stops with code 5
#[test]
fn signal_after_handshake_stops_all_agents() {
    block_on_local(async {
        let signals = signal_sender();
        let launcher = LocalProcessLauncher::new(&[("AGENT_EOF", "5")]);
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        client.new_session(3, "/projects/b").await;
        signals.send(libc::SIGTERM).unwrap();
        assert_eq!(client.proxy_end().await, 5);
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
        let launcher = LocalProcessLauncher::new(&[("AGENT_EOF", "ignore")]);
        let mut config = test_config(true);
        config.shutdown_grace = Duration::from_secs(5);
        let mut client = TestClient::session(launcher, config, "/projects/a").await;
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
