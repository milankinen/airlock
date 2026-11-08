//! Tests for the session that a load opens, with the fake agent process.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that an agent cannot take the open session of another project with
/// its answer to a load. The agent is not trusted. The session of the
/// other project must keep its agent.
///   1. Start the session `b:s1` in sandbox `b`
///   2. Load the session `a:x` in sandbox `a` with an agent that answers the
///      load with the session id `s1`
///   3. Send a prompt to `b:s1` and check that the agent in `b` gets it
///   4. Send a prompt to `a:x` and check that the agent in `a` gets it
#[test]
fn load_answer_with_other_session_id_does_not_take_that_session() {
    block_on_local(async {
        let env = [("AGENT_SESSION_IDS", "1"), ("AGENT_LOAD_SID", "s1")];
        let mut client = TestClient::start(LocalProcessLauncher::new(&env), test_config(true));
        client.initialize(json!({})).await;
        assert_eq!(client.new_session(1, "/projects/b").await, "b:s1");
        let params = json!({ "sessionId": "a:x", "cwd": "/projects/a", "mcpServers": [] });
        let (response, _) = client.request(2, "session/load", params).await;
        assert!(response.get("result").is_some(), "{response}");

        for (id, sid, sandbox) in [(3, "b:s1", "b"), (4, "a:x", "a")] {
            let (response, others) = client.prompt(id, sid).await;
            assert!(response.get("result").is_some(), "{response}");
            let from: Vec<&Value> = others
                .iter()
                .filter(|m| m.pointer("/params/seen/method") == Some(&json!("session/prompt")))
                .map(|m| &m["params"]["from"])
                .collect();
            assert_eq!(from, [sandbox], "{others:?}");
        }
    });
}
