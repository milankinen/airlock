//! Tests for the handshake before the agent runs and the agent start at
//! the first session.

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that the proxy answers the handshake before the agent runs, and
/// starts the agent at the first session with only the allowed client
/// features and the guest session dir. The editor sends the project dir
/// only in the session request, and the agent must not get host features.
///   1. Send `initialize` with all client features and check the proxy
///      result and that no agent started
///   2. Send `session/new` in a sub dir of sandbox `a`, with editor MCP
///      servers
///   3. Check that the agent got version 1 handshake params with only the
///      allowed features
///   4. Check that the agent got the guest dir and no editor MCP servers:
///      the MCP bridge is off
///   5. Send `initialize` again and check that the proxy answers it and
///      the agent does not get it
#[test]
fn first_session_starts_agent_with_filtered_handshake() {
    block_on_local(async {
        let starter = LocalStarter::new(&[]);
        let starts = starter.starts.clone();
        let mut client = TestClient::start(starter, test_config(true));

        let caps = json!({
            "fs": { "readTextFile": true, "writeTextFile": true },
            "terminal": true,
            "auth": { "terminal": true },
            "elicitation": { "form": {}, "url": {} },
            "session": {},
            "_meta": { "x": 1 },
        });
        let result = client.initialize(caps).await;
        assert_eq!(result["protocolVersion"], 1);
        assert!(result["agentCapabilities"].get("mcpCapabilities").is_none());
        assert_eq!(result["agentInfo"]["title"], "fake-agent (airlock)");
        assert_eq!(starts.get(), 0);

        let servers = json!([
            { "name": "local", "command": "tool", "args": [], "env": [] },
            { "type": "acp", "name": "editor", "id": "e1" },
        ]);
        let params = json!({ "cwd": "/projects/a/sub", "mcpServers": servers });
        let (response, others) = client.request(2, "session/new", params).await;
        assert_eq!(response["result"], json!({}));
        assert_eq!(starts.get(), 1);

        let got = seen(&others);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0]["params"]["protocolVersion"], 1);
        assert_eq!(
            got[0]["params"]["clientCapabilities"],
            json!({ "auth": {}, "elicitation": { "form": {} }, "session": {} })
        );
        assert_eq!(got[1]["method"], "session/new");
        assert_eq!(got[1]["params"]["cwd"], "/work/sub");
        assert_eq!(got[1]["params"]["mcpServers"], json!([]));

        let (response, _) = client.request(3, "initialize", json!({})).await;
        assert_eq!(response["result"]["agentInfo"]["name"], "airlock");
        let prompt = json!({ "sessionId": "s", "prompt": [] });
        let (_, others) = client.request(4, "session/prompt", prompt).await;
        let got = seen(&others);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0]["method"], "session/prompt");
        assert_eq!(starts.get(), 1);
    });
}

/// Test that a request before a session gets an error and starts no
/// agent. The agent can start only when the session dir is known.
///   1. Send a session request before `initialize` and check the error
///   2. Send `initialize`, then a prompt, and check the error
///   3. Check that no agent started
#[test]
fn request_before_session_is_refused() {
    block_on_local(async {
        let starter = LocalStarter::new(&[]);
        let starts = starter.starts.clone();
        let mut client = TestClient::start(starter, test_config(true));

        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, _) = client.request(1, "session/new", params).await;
        assert_eq!(response["error"]["code"], -32600);

        client.initialize(json!({})).await;
        let prompt = json!({ "sessionId": "s", "prompt": [] });
        let (response, _) = client.request(2, "session/prompt", prompt).await;
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(starts.get(), 0);
        assert_eq!(client.exit_code().await, 0);
    });
}

/// Test that an agent response with the handshake id after the handshake
/// goes to the client as any other response. Only the first reply
/// completes the handshake. A later one must not change the agent state.
///   1. Use an agent that sends a second response with the handshake id
///      after the handshake
///   2. Start a session and check that the client got that response
///   3. Send a prompt and check that the same agent answers it
#[test]
fn agent_response_with_handshake_id_after_handshake_goes_to_client() {
    let say = r#"{"jsonrpc":"2.0","id":"airlock-acp-initialize","result":{"again":true}}"#;
    block_on_local(async {
        let starter = LocalStarter::new(&[("AGENT_SAY", say)]);
        let starts = starter.starts.clone();
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, others) = client.request(2, "session/new", params).await;
        assert!(response.get("result").is_some(), "{response}");
        let again: Vec<_> = others
            .iter()
            .filter(|m| m["id"] == "airlock-acp-initialize")
            .collect();
        assert_eq!(again.len(), 1, "{others:?}");
        assert_eq!(again[0]["result"], json!({ "again": true }));

        let prompt = json!({ "sessionId": "s", "prompt": [] });
        let (response, _) = client.request(3, "session/prompt", prompt).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(starts.get(), 1);
    });
}
