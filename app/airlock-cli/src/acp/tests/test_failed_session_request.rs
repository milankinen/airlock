//! Tests for session requests that start no session, with the fake agent
//! process.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use serde_json::json;

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, full_agent_init, test_config};
use crate::test_cfg::block_on_local;

/// Wait until `count` agent processes exited. Panics after 10 seconds.
async fn wait_exited(exited: &Rc<Cell<usize>>, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while exited.get() < count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent exit in time");
}

/// Test that a session request that the agent or the proxy refuses stops
/// its agent. Else the agent and its sandbox keep running without a
/// session.
///   1. Send `session/load` to an agent that refuses all requests
///   2. Send `session/resume` to an agent that cannot resume. The proxy
///      refuses it
///   3. Check the error of each, and that each agent exits
#[test]
fn refused_session_request_stops_agent() {
    block_on_local(async {
        let mut init = full_agent_init();
        init["agentCapabilities"]["sessionCapabilities"]
            .as_object_mut()
            .unwrap()
            .remove("resume");
        let init = init.to_string();
        let error = r#"{"code":-32002,"message":"Resource not found"}"#;
        let cases = [
            ("session/load", ("AGENT_ERROR", error)),
            ("session/resume", ("AGENT_INIT", init.as_str())),
        ];
        for (method, env) in cases {
            let launcher = LocalProcessLauncher::new(&[env]);
            let exited = launcher.exited.clone();
            let mut client = TestClient::start(launcher, test_config(true));
            client.initialize(json!({})).await;
            let params = json!({ "sessionId": "x", "cwd": "/projects/a", "mcpServers": [] });
            let (response, _) = client.request(1, method, params).await;
            assert!(response.get("error").is_some(), "{method}: {response}");
            wait_exited(&exited, 1).await;
        }
    });
}

/// Test that a new session with the id of an open session of another agent
/// stops the new agent. The proxy refuses the second session, and its
/// agent has nothing to do.
///   1. Use agents that give each new session the id `s1`
///   2. Start two sessions in sandbox `a`
///   3. Check that the second one gets an error and its agent exits
///   4. Check that a prompt to `s1` still gets its answer
#[test]
fn session_id_conflict_stops_new_agent() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_SESSION_IDS", "1")]);
        let exited = launcher.exited.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        assert_eq!(client.new_session(1, "/projects/a").await, "s1");
        let (_, message) = client.session_error(2, "/projects/a/x").await;
        assert!(message.contains("s1"), "{message}");
        wait_exited(&exited, 1).await;

        let (response, _) = client.prompt(3, "s1").await;
        assert_eq!(response["result"], json!({}), "{response}");
    });
}
