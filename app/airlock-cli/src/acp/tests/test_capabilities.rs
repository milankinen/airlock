//! Tests for the client requests for features that the agent does not
//! have.

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that the proxy refuses the client requests for the features that
/// it gave in its handshake reply but the real agent does not have. The
/// proxy replies to the handshake before the agent runs.
///   1. Use an agent without features
///   2. Start a session
///   3. Send `session/load` and a prompt with an image, and check the
///      errors
///   4. Send a text prompt and check that the agent got only it
#[test]
fn requests_for_missing_agent_features_are_refused() {
    block_on_local(async {
        let init = json!({ "protocolVersion": 1, "agentCapabilities": {} }).to_string();
        let starter = LocalStarter::new(&[("AGENT_INIT", &init)]);
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        client.request(2, "session/new", params).await;

        let params = json!({ "sessionId": "s", "cwd": "/projects/a", "mcpServers": [] });
        let (response, mut others) = client.request(3, "session/load", params).await;
        assert_eq!(response["error"]["code"], -32601);
        let image = json!({ "type": "image", "data": "", "mimeType": "image/png" });
        let prompt = json!({ "sessionId": "s", "prompt": [image] });
        let (response, more) = client.request(4, "session/prompt", prompt).await;
        others.extend(more);
        assert_eq!(response["error"]["code"], -32602);

        let text = json!({ "type": "text", "text": "hi" });
        let prompt = json!({ "sessionId": "s", "prompt": [text] });
        let (response, more) = client.request(5, "session/prompt", prompt).await;
        others.extend(more);
        assert!(response.get("result").is_some(), "{response}");
        let ids: Vec<_> = seen(&others).iter().map(|m| m["id"].clone()).collect();
        assert_eq!(ids, [json!(5)]);
    });
}
