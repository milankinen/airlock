//! Tests for session dirs out of the sandbox of the running agent.

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that the proxy refuses a session dir that is not in the sandbox of
/// the running agent, and maps a dir in the sandbox to the guest. One
/// agent runs in one sandbox and cannot see the files of other projects.
///   1. Send a session request with a relative dir before the agent runs
///      and check the error
///   2. Start the agent with a session in sandbox `a`
///   3. Send session requests in sandbox `b`, in a dir without a sandbox
///      and in a relative dir, and check the errors
///   4. Load a session in a sub dir of sandbox `a` and check that the
///      agent got the guest dir
///   5. Check that the agent got none of the refused requests
#[test]
fn session_out_of_agent_sandbox_is_refused() {
    block_on_local(async {
        let starter = LocalStarter::new(&[]);
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;

        let (response, _) = client
            .request(
                1,
                "session/new",
                json!({ "cwd": "projects/a", "mcpServers": [] }),
            )
            .await;
        assert_eq!(response["error"]["code"], -32602);

        let (response, _) = client
            .request(
                2,
                "session/new",
                json!({ "cwd": "/projects/a", "mcpServers": [] }),
            )
            .await;
        assert!(response.get("result").is_some(), "{response}");

        let mut seen_all = Vec::new();
        for (id, cwd, code) in [
            (3, "/projects/b", -32602),
            (4, "/nowhere", -32603),
            (5, "projects/a", -32602),
        ] {
            let params = json!({ "cwd": cwd, "mcpServers": [] });
            let (response, others) = client.request(id, "session/new", params).await;
            assert_eq!(response["error"]["code"], code, "{cwd}: {response}");
            seen_all.extend(seen(&others));
        }
        let (_, message) = client.session_error(6, "/projects/b/x").await;
        assert!(message.contains("not in the sandbox"), "{message}");

        let params = json!({ "sessionId": "s", "cwd": "/projects/a/sub", "mcpServers": [] });
        let (_, others) = client.request(7, "session/load", params).await;
        seen_all.extend(seen(&others));
        let methods: Vec<_> = seen_all.iter().map(|m| m["id"].clone()).collect();
        assert_eq!(methods, [json!(7)]);
        assert_eq!(seen_all[0]["params"]["cwd"], "/work/sub");
    });
}
