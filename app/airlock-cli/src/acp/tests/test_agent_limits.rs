//! Tests for the limits of the agent ids that the proxy keeps in memory,
//! with the fake agent process.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that an agent cannot fill the host memory with new session ids.
/// The agent is not trusted.
///   1. Use an agent that sends updates in a session with a too long id
///      and in 70 new sessions
///   2. Start a session
///   3. Check that the client got updates in only 64 new sessions, and
///      none in the session with the long id
#[test]
fn agent_gets_limited_number_of_new_sessions() {
    block_on_local(async {
        let update = |sid: &str| {
            json!({ "jsonrpc": "2.0", "method": "session/update",
                    "params": { "sessionId": sid, "update": {} } })
            .to_string()
        };
        let long = "x".repeat(300);
        let mut lines = vec![update(&long)];
        lines.extend((0..70).map(|i| update(&format!("child-{i}"))));
        let say = lines.join("\n");
        let launcher = LocalProcessLauncher::new(&[("AGENT_SAY", &say)]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, others) = client.request(1, "session/new", params).await;
        assert!(response.get("result").is_some(), "{response}");

        let sessions: Vec<&str> = others
            .iter()
            .filter_map(|m| m.pointer("/params/sessionId").and_then(Value::as_str))
            .collect();
        assert_eq!(sessions.len(), 64, "{sessions:?}");
        assert!(sessions.iter().all(|s| s.starts_with("child-")));
    });
}

/// Test that the proxy refuses agent requests with a too long id, and
/// agent requests over the limit of open requests. The proxy keeps the
/// open requests in memory.
///   1. Use an agent that sends a request with a too long id, and 65
///      requests that get no answer
///   2. Start a session and send a prompt
///   3. Check that the client got 64 requests, and that the agent got an
///      error for the long id and for the last request
#[test]
fn agent_requests_over_limits_are_refused() {
    block_on_local(async {
        let ask = |id: Value| {
            json!({ "jsonrpc": "2.0", "id": id,
                    "method": "session/request_permission", "params": {} })
            .to_string()
        };
        let long = "y".repeat(300);
        let mut lines = vec![ask(json!(long))];
        lines.extend((1..=65).map(|i| ask(json!(i))));
        let say = lines.join("\n");
        let launcher = LocalProcessLauncher::new(&[("AGENT_SAY", &say)]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, mut all) = client.request(1, "session/new", params).await;
        let sid = response["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let (_, more) = client.prompt(2, &sid).await;
        all.extend(more);

        let asks = all
            .iter()
            .filter(|m| m["method"] == "session/request_permission")
            .count();
        assert_eq!(asks, 64);
        let refused: Vec<Value> = seen(&all)
            .into_iter()
            .filter(|m| m.get("error").is_some())
            .map(|m| m["id"].clone())
            .collect();
        assert_eq!(refused, [json!(long), json!(65)]);
    });
}
