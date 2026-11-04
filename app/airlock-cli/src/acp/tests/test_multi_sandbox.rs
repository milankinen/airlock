//! Tests for agents in many sandboxes, with the fake agent process.

use serde_json::json;

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, seen_from, test_config};
use crate::test_cfg::block_on_local;

/// Test that sessions in two sandboxes run in two agent processes, and
/// that the prompts go to the agent of their session. One editor can open
/// projects in many sandboxes.
///   1. Start sessions in sandboxes `a` and `b`
///   2. Send a prompt to each session
///   3. Check that each agent got only the prompt of its session
#[test]
fn sessions_in_two_sandboxes_run_in_own_agents() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]);
        let launched = launcher.launched.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let sa = client.new_session(1, "/projects/a").await;
        let sb = client.new_session(2, "/projects/b").await;
        assert_eq!(*launched.borrow(), ["a", "b"]);

        let (_, mut others) = client.prompt(3, &sa).await;
        others.extend(client.prompt(4, &sb).await.1);
        let ids = |sandbox| -> Vec<_> {
            seen_from(&others, sandbox)
                .iter()
                .filter(|m| m["method"] == "session/prompt")
                .map(|m| m["id"].clone())
                .collect()
        };
        assert_eq!(ids("a"), [json!(3)]);
        assert_eq!(ids("b"), [json!(4)]);
    });
}

/// Test that an agent exit tells the client about the end of its session,
/// and that the other session and the proxy keep running. The editor must
/// not wait for answers that never come.
///   1. Start sessions in sandboxes `a` and `b`, with and without the
///      session notice feature of the client
///   2. Send a request to `a` that gets no reply, and make agent `a` exit
///      with code 7
///   3. Check that the request gets an error and the session gets a notice
///      or an agent message
///   4. Check that a prompt to `b` gets its answer
///   5. Close the client and check the exit code 7
#[test]
fn agent_exit_ends_its_session_and_keeps_others() {
    block_on_local(async {
        for notices in [true, false] {
            let caps = if notices {
                json!({ "session": { "notices": {} } })
            } else {
                json!({})
            };
            let mut client = TestClient::start(LocalProcessLauncher::new(&[]), test_config(true));
            client.initialize(caps).await;
            let sa = client.new_session(1, "/projects/a").await;
            let sb = client.new_session(2, "/projects/b").await;

            client
                .send_request(3, "test/hold", json!({ "sessionId": sa }))
                .await;
            client.notify("test/exit", json!({ "sessionId": sa })).await;
            let (response, others) = client.response(3).await;
            assert_eq!(response["error"]["code"], -32603, "{response}");
            let mut others = others;
            others.push(client.recv().await);
            let update = others
                .iter()
                .find(|m| m.pointer("/params/update").is_some())
                .unwrap_or_else(|| panic!("{others:?}"));
            assert_eq!(update["params"]["sessionId"], json!(sa));
            let kind = &update["params"]["update"]["sessionUpdate"];
            assert_eq!(
                kind,
                if notices {
                    "notice"
                } else {
                    "agent_message_chunk"
                }
            );

            let (response, _) = client.prompt(4, &sb).await;
            assert!(response.get("result").is_some(), "{response}");
            assert_eq!(client.exit_code().await, 7);
        }
    });
}
