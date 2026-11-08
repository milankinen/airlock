//! Tests for the client requests for features that the agent does not
//! have.

use serde_json::json;

use crate::test_cfg::acp::{
    LocalProcessLauncher, TestClient, full_agent_init, request_ids, seen, test_config,
};
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
        let launcher = LocalProcessLauncher::new(&[("AGENT_INIT", &init)]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let sid = client.new_session(2, "/projects/a").await;

        // A load of the open session would stop its agent.
        let params = json!({ "sessionId": "a:other", "cwd": "/projects/a", "mcpServers": [] });
        let (response, mut others) = client.request(3, "session/load", params).await;
        assert_eq!(response["error"]["code"], -32601);
        let image = json!({ "type": "image", "data": "", "mimeType": "image/png" });
        let prompt = json!({ "sessionId": sid, "prompt": [image] });
        let (response, more) = client.request(4, "session/prompt", prompt).await;
        others.extend(more);
        assert_eq!(response["error"]["code"], -32602);

        let text = json!({ "type": "text", "text": "hi" });
        let prompt = json!({ "sessionId": sid, "prompt": [text] });
        let (response, more) = client.request(5, "session/prompt", prompt).await;
        others.extend(more);
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(request_ids(&seen(&others)), [json!(5)]);
    });
}

/// Test that the proxy offers steering in its handshake reply, and refuses
/// `_session/steering` when the real agent does not have it. JetBrains
/// IDEs use the offer to enable "Send now".
///   1. Start a session with an agent without steering
///   2. Check the steering offer in the handshake reply
///   3. Send a steering request and check that it gets
///      `METHOD_NOT_FOUND` and does not reach the agent
///   4. Send a text prompt and check that the agent got only it
///   5. Start a session with an agent that has steering
///   6. Send a text steering request and check that the agent gets it
///   7. Send a steering request with an image and check that it is
///      refused, because the agent has no image support
#[test]
fn steering_request_follows_agent_steering_support() {
    block_on_local(async {
        let steer = |sid: &str, content: serde_json::Value| json!({ "sessionId": sid, "prompt": [content] });
        let text = json!({ "type": "text", "text": "hi" });
        let image = json!({ "type": "image", "data": "", "mimeType": "image/png" });

        let init = json!({ "protocolVersion": 1, "agentCapabilities": {} }).to_string();
        let launcher = LocalProcessLauncher::new(&[("AGENT_INIT", &init)]);
        let mut client = TestClient::start(launcher, test_config(true));
        let result = client.initialize(json!({})).await;
        assert_eq!(result["_meta"]["steering"]["supported"], true);
        let sid = client.new_session(2, "/projects/a").await;
        let (response, others) = client
            .request(3, "_session/steering", steer(&sid, text.clone()))
            .await;
        assert_eq!(response["error"]["code"], -32601);
        let (response, more) = client
            .request(4, "session/prompt", steer(&sid, text.clone()))
            .await;
        assert!(response.get("result").is_some(), "{response}");
        let mut others = others;
        others.extend(more);
        assert_eq!(request_ids(&seen(&others)), [json!(4)]);

        let mut init = full_agent_init();
        init["_meta"] = json!({ "steering": { "supported": true } });
        init["agentCapabilities"]["promptCapabilities"] = json!({});
        let init = init.to_string();
        let launcher = LocalProcessLauncher::new(&[("AGENT_INIT", &init)]);
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        let sid = client.sid.clone();
        let (response, others) = client
            .request(3, "_session/steering", steer(&sid, text))
            .await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(request_ids(&seen(&others)), [json!(3)]);
        let (response, _) = client
            .request(4, "_session/steering", steer(&sid, image))
            .await;
        assert_eq!(response["error"]["code"], -32602);
    });
}

/// Test that the proxy refuses a session request that the real agent does
/// not have, also when the proxy offered it. Most session features are
/// empty objects, thus the check takes objects as present.
///   1. Use an agent with the session list and no session fork
///   2. Start a session
///   3. Send `session/fork`, which starts its own agent, and check that it
///      gets `METHOD_NOT_FOUND` and does not reach the agent
///   4. Send a prompt and check that the agent got it
#[test]
fn session_fork_is_refused_when_agent_lacks_it() {
    block_on_local(async {
        let mut init = full_agent_init();
        init["agentCapabilities"]["sessionCapabilities"] = json!({ "list": {} });
        let init = init.to_string();
        let launcher = LocalProcessLauncher::new(&[("AGENT_INIT", &init)]);
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        let params = json!({ "sessionId": client.sid, "cwd": "/projects/a", "mcpServers": [] });
        let (response, mut others) = client.request(3, "session/fork", params).await;
        assert_eq!(response["error"]["code"], -32601);
        let sid = client.sid.clone();
        let (response, more) = client.prompt(4, &sid).await;
        assert!(response.get("result").is_some(), "{response}");
        others.extend(more);
        assert_eq!(request_ids(&seen(&others)), [json!(4)]);
    });
}
