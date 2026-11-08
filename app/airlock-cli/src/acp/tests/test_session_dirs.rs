//! Tests for the session dirs of the session requests.

use serde_json::json;

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that each session request gets an agent in the sandbox of its
/// session dir, with the guest dir, and that dirs without a sandbox or not
/// absolute are refused. One client can open projects in many sandboxes.
///   1. Send session requests in sandboxes `a` and `b`, and check that
///      each launched an agent in its sandbox
///   2. Send session requests in a dir without a sandbox and in a relative
///      dir, and check the errors
///   3. Load a session in a sub dir of sandbox `a` and check that its new
///      agent in `a` got the guest dir
#[test]
fn session_requests_use_agents_in_sandbox_of_session_dir() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]);
        let launched = launcher.launched.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;

        client.new_session(1, "/projects/a").await;
        client.new_session(2, "/projects/b/x").await;
        assert_eq!(*launched.borrow(), ["a", "b"]);

        for (id, cwd, code) in [(3, "/nowhere", -32603), (4, "projects/a", -32602)] {
            let (got, _) = client.session_error(id, cwd).await;
            assert_eq!(got, code, "{cwd}");
        }

        let params = json!({ "sessionId": "a:old", "cwd": "/projects/a/sub", "mcpServers": [] });
        let (response, others) = client.request(5, "session/load", params).await;
        assert!(response.get("result").is_some(), "{response}");
        let load = seen(&others)
            .into_iter()
            .find(|m| m["id"] == 5)
            .expect("load request");
        assert_eq!(load["params"]["cwd"], "/work/sub");
        assert_eq!(*launched.borrow(), ["a", "b", "a"]);
    });
}
