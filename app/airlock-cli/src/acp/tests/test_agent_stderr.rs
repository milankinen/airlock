//! Tests for the agent stderr relay.

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that an editor that never reads the proxy stderr does not stop the
/// relay or the exit. The proxy drops the agent stderr that does not fit.
///   1. Use an agent that writes 4 MiB to stderr before the handshake
///      reply, and a client that never reads stderr
///   2. Start a session and send a prompt
///   3. Check that the client gets the agent replies
///   4. Close the client and check that the proxy stops with the agent
///      exit code
#[test]
fn agent_stderr_that_nobody_reads_does_not_stop_relay() {
    block_on_local(async {
        let starter = LocalStarter::new(&[("AGENT_STDERR", "4194304")]);
        let mut client = TestClient::session(starter, test_config(true), "/projects/a").await;
        let params = json!({ "sessionId": "s1", "prompt": [] });
        let (response, _) = client.request(3, "session/prompt", params).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(client.exit_code().await, 0);
    });
}
