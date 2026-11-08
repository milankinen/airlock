//! Tests for the ACP session ids in agent messages, with the fake agent
//! process.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, input_log, test_config};
use crate::test_cfg::{block_on_local, temp_dir};

/// Test that nested session id fields that are not strings, for example in
/// tool input, do not stop agent messages, and that a client answer to an
/// agent request reaches the agent as it is. Else agent requests wait
/// forever and answers are lost. An agent message whose own session id is
/// not a string names no session and must not reach the client.
///   1. Start a session with an agent that sends an update and a
///      permission request with nested session ids that are not strings,
///      and an update with a session id that is not a string
///   2. Check that the client got the first two, with the nested values as
///      they are, and not the last update
///   3. Answer the request with a nested session id
///   4. Check that the agent got the answer as the client sent it
#[test]
fn nested_session_ids_of_any_type_pass() {
    block_on_local(async {
        let say = [
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s9","update":{"rawInput":{"sessionId":3}}}}"#,
            r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s9","options":[],"toolCall":{"rawInput":{"sessionId":null}}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":5,"update":{}}}"#,
        ]
        .join("\n");
        let dir = temp_dir();
        let log = dir.path().join("input");
        let env = [
            ("AGENT_SAY", say.as_str()),
            ("AGENT_INPUT_LOG", log.to_str().unwrap()),
        ];
        let mut client = TestClient::start(LocalProcessLauncher::new(&env), test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (_, others) = client.request(1, "session/new", params).await;
        let update = others
            .iter()
            .find(|m| {
                m["method"] == "session/update" && m.pointer("/params/update/rawInput").is_some()
            })
            .unwrap_or_else(|| panic!("{others:?}"));
        assert_eq!(update["params"]["update"]["rawInput"]["sessionId"], 3);
        assert!(
            others.iter().all(|m| m["params"]["sessionId"] != 5),
            "{others:?}"
        );
        let ask = others
            .iter()
            .find(|m| m["method"] == "session/request_permission")
            .unwrap_or_else(|| panic!("{others:?}"));
        assert_eq!(
            ask["params"]["toolCall"]["rawInput"]["sessionId"],
            Value::Null
        );

        let result =
            json!({ "outcome": { "outcome": "cancelled" }, "_meta": { "sessionId": "x" } });
        client
            .send(&json!({ "jsonrpc": "2.0", "id": ask["id"], "result": result }))
            .await;
        client.prompt(2, "s9").await;
        let answer = input_log(&log)
            .into_iter()
            .find(|m| m["id"] == 7 && m.get("result").is_some())
            .expect("answer in the agent input");
        assert_eq!(answer["result"], result);
    });
}
