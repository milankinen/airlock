//! Tests for the ids of the agent requests, with the fake agent process.

use serde_json::{Value, json};

use crate::acp::rpc::CANCEL_REQUEST;
use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Get the first `session/request_permission` in `msgs`. Panics if there
/// is none.
fn permission_request(msgs: &[Value]) -> Value {
    msgs.iter()
        .find(|m| m["method"] == "session/request_permission")
        .unwrap_or_else(|| panic!("{msgs:?}"))
        .clone()
}

/// Test that two agents can use the same request id. The client must see
/// different ids, and each answer must reach its agent with the original
/// id. Else an answer goes to the wrong agent.
///   1. Start sessions in sandboxes `a` and `b`, with agents that ask for
///      a permission with the id 1 at each prompt
///   2. Send a prompt to each session and check that the client got two
///      different ids
///   3. Answer both requests and check that each agent got its answer with
///      the id 1
#[test]
fn same_request_id_of_two_agents_is_kept_apart() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_ASK", "1")]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let mut asks = Vec::new();
        for (id, cwd) in [(1, "/projects/a"), (2, "/projects/b")] {
            let sid = client.new_session(id, cwd).await;
            let (_, others) = client.prompt(id + 10, &sid).await;
            asks.push(permission_request(&others));
        }
        assert_ne!(asks[0]["id"], asks[1]["id"]);

        for (ask, option) in asks.iter().zip(["a", "b"]) {
            let result = json!({ "outcome": { "outcome": "selected", "optionId": option } });
            client
                .send(&json!({ "jsonrpc": "2.0", "id": ask["id"], "result": result }))
                .await;
        }
        let mut answers = Vec::new();
        while answers.len() < 2 {
            let msg = client.recv().await;
            if let Some(seen) = msg.pointer("/params/seen")
                && seen.get("result").is_some()
            {
                assert_eq!(seen["id"], 1, "{seen}");
                let option = &seen["result"]["outcome"]["optionId"];
                answers.push((msg["params"]["from"].clone(), option.clone()));
            }
        }
        answers.sort_by_key(|(from, _)| from.to_string());
        assert_eq!(
            answers,
            [(json!("a"), json!("a")), (json!("b"), json!("b"))]
        );
    });
}

/// Test that an agent can cancel only its own open requests, and that the
/// client gets the cancel with the id that it knows. Else an agent can
/// close a dialog of another session.
///   1. Start a session with an agent that asks for a permission with the
///      id 5 at each prompt, and send a prompt
///   2. Make the agent cancel the id 9 and then the id 5
///   3. Check that the client gets only the cancel of the id 5, with the
///      client id of the permission request
#[test]
fn agent_cancel_reaches_client_only_for_own_request() {
    block_on_local(async {
        let cancels = [9, 5]
            .map(|id| format!(r#"{{"jsonrpc":"2.0","method":"$/cancel_request","params":{{"requestId":{id}}}}}"#))
            .join("\n");
        let env = [("AGENT_ASK", "5"), ("AGENT_ON_HOLD", cancels.as_str())];
        let launcher = LocalProcessLauncher::new(&env);
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        let sid = client.sid.clone();
        let (_, others) = client.prompt(3, &sid).await;
        let ask = permission_request(&others);

        client
            .send_request(4, "test/hold", json!({ "sessionId": sid }))
            .await;
        let cancel = loop {
            let msg = client.recv().await;
            if msg["method"] == CANCEL_REQUEST {
                break msg;
            }
        };
        assert_eq!(cancel["params"]["requestId"], ask["id"], "{cancel}");
    });
}
