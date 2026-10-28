//! Tests for the limit of the agent output line.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::test_cfg::acp::{FAKE_AGENT, LocalStarter, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that the proxy stops an agent that writes a line over the limit,
/// in both modes. The proxy keeps a line in memory until its end. Else the
/// agent can fill the host memory.
///   1. Use an agent that writes a long line before the handshake
///   2. Check that the session fails with the line limit error
///   3. Use an agent that writes a line without end after the handshake
///   4. Check that the proxy stops with the kill exit code, also when the
///      client stays open
#[test]
fn agent_output_line_over_limit_stops_agent() {
    let long = "x".repeat(70 * 1024);
    block_on_local(async {
        for restrict in [true, false] {
            let starter = LocalStarter::new(&[("AGENT_PRE", &long)]);
            let mut client = TestClient::start(starter, test_config(restrict));
            client.initialize(json!({})).await;
            let (code, message) = client.session_error(2, "/projects/a").await;
            assert_eq!(code, -32603);
            assert!(message.contains("longer than 65536 bytes"), "{message}");

            let starter = LocalStarter::new(&[("AGENT_FLOOD", "1")]);
            let mut client = TestClient::start(starter, test_config(restrict));
            client.initialize(json!({})).await;
            let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
            let msg =
                json!({ "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": params });
            client.send(&msg).await;
            assert_eq!(client.proxy_end().await, 128 + libc::SIGKILL);
        }
    });
}

/// Test that the proxy relays a long agent line near the limit in a short
/// time. The proxy reads the agent output in small parts. A search of the
/// full partial line for each part takes quadratic time and blocks the
/// proxy.
///   1. Use an agent that writes a 12 MiB message before the handshake
///      reply, with a 16 MiB line limit
///   2. Start a session
///   3. Check that the client gets the full message, and that the session
///      starts in less than 5 seconds
#[test]
fn long_agent_line_near_limit_is_relayed_in_short_time() {
    const LEN: usize = 12 * 1024 * 1024;
    let script = format!(
        "printf '{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"big\":\"'
head -c {LEN} /dev/zero | tr '\\0' x
printf '\"}}}}\\n'
{FAKE_AGENT}"
    );
    let mut config = test_config(true);
    config.argv = vec!["sh".into(), "-c".into(), script];
    config.max_line_bytes = 16 * 1024 * 1024;
    block_on_local(async {
        let mut client = TestClient::start(LocalStarter::new(&[]), config);
        client.initialize(json!({})).await;
        let started = Instant::now();
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, others) = client.request(2, "session/new", params).await;
        let elapsed = started.elapsed();
        assert!(response.get("result").is_some(), "{response}");
        let big = others
            .iter()
            .find_map(|m| m.pointer("/params/big").and_then(|b| b.as_str()))
            .expect("long message");
        assert_eq!(big.len(), LEN);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    });
}
