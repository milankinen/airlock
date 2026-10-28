//! Tests for agents that need a login.

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that the client gets a login hint when the agent needs a login.
/// The proxy gives no login methods and no login terminal, so the user
/// must log in inside the sandbox.
///   1. Use an agent that replies to each request with the
///      `auth_required` error
///   2. Start a session
///   3. Check that the error has the agent message and the hint
#[test]
fn auth_required_error_gets_login_hint() {
    block_on_local(async {
        let error = json!({ "code": -32000, "message": "Authentication required" }).to_string();
        let starter = LocalStarter::new(&[("AGENT_ERROR", &error)]);
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, _) = client.request(2, "session/new", params).await;
        assert_eq!(response["error"]["code"], -32000);
        let message = response["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("Authentication required. "),
            "{message}"
        );
        assert!(message.contains("airlock exec -l fake-agent"), "{message}");
    });
}
