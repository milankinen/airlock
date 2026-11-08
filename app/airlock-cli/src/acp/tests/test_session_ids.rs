//! Tests for the sandbox prefix of the ACP session ids, with the fake agent
//! process.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, input_log, test_config};
use crate::test_cfg::{block_on_local, temp_dir};

/// Get the input message with the id `id` from the agent input log
/// `path`. Panics if there is none.
fn agent_got(path: &std::path::Path, id: i64) -> Value {
    let input = input_log(path);
    input
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{input:?}"))
        .clone()
}

/// Test that the client sees each agent session id with the sandbox of the
/// agent as prefix, also in nested fields, and that the agent gets the ids
/// without it. The agents of two projects can know the same session ids,
/// for example from a shared agent config. An agent must not take a
/// session of another project.
///   1. Start a session in sandbox `a` with an agent that sends an update
///      in the session `s9` with the parent session `p1`, and an update
///      with a session id that is not a string
///   2. Check that the client got only the first update, in `a:s9` with
///      the parent `a:p1`
///   3. Send a prompt to `a:s9` with the parent `a:p1`, and check that the
///      agent got `s9` and `p1`
///   4. Send a prompt to `a:s9` with the parent `b:p1`, and check that the
///      agent got the parent as it is. Only the session of the message
///      selects the agent.
#[test]
fn agent_session_ids_get_sandbox_prefix() {
    block_on_local(async {
        let say = [
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s9","update":{"parentSessionId":"p1"}}}"#,
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
        let updates: Vec<&Value> = others
            .iter()
            .filter(|m| m.pointer("/params/update").is_some())
            .collect();
        assert_eq!(updates.len(), 1, "{others:?}");
        assert_eq!(updates[0]["params"]["sessionId"], "a:s9");
        assert_eq!(updates[0]["params"]["update"]["parentSessionId"], "a:p1");

        let prompt = |parent: &str| json!({ "sessionId": "a:s9", "prompt": [], "_meta": { "parentSessionId": parent } });
        let (response, _) = client.request(2, "session/prompt", prompt("a:p1")).await;
        assert_eq!(response["result"], json!({}), "{response}");
        let seen = agent_got(&log, 2);
        assert_eq!(seen["params"]["sessionId"], "s9");
        assert_eq!(seen["params"]["_meta"]["parentSessionId"], "p1");

        let (response, _) = client.request(3, "session/prompt", prompt("b:p1")).await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert_eq!(
            agent_got(&log, 3)["params"]["_meta"]["parentSessionId"],
            "b:p1"
        );
    });
}

/// Test that nested session id fields that are not strings, for example in
/// tool input, do not stop agent messages, and that a client answer to an
/// agent request reaches the agent as it is. Else agent requests wait
/// forever and answers are lost.
///   1. Start a session with an agent that sends an update and a
///      permission request with nested session ids that are not strings
///   2. Check that the client got both, with the nested values as they are
///   3. Answer the request with a nested session id
///   4. Check that the agent got the answer as the client sent it
#[test]
fn nested_session_ids_of_any_type_pass() {
    block_on_local(async {
        let say = [
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s9","update":{"rawInput":{"sessionId":3}}}}"#,
            r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s9","options":[],"toolCall":{"rawInput":{"sessionId":null}}}}"#,
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
        client.prompt(2, "a:s9").await;
        let answer = input_log(&log)
            .into_iter()
            .find(|m| m["id"] == 7 && m.get("result").is_some())
            .expect("answer in the agent input");
        assert_eq!(answer["result"], result);
    });
}

/// Test that the rows of a session list have the sandbox prefix, and that
/// a delete and a load of a listed session start an agent in the dir of
/// the list, which gets the id without the prefix.
///   1. Send `session/list` in sandbox `a` with an agent that lists `old`
///   2. Check that the row is `a:old`
///   3. Send `session/delete` and `session/load` for `a:old`
///   4. Check that the agents got `old`
#[test]
fn listed_session_ids_get_sandbox_prefix() {
    block_on_local(async {
        let list = json!({ "sessions": [{ "sessionId": "old", "cwd": "/work" }] }).to_string();
        let dir = temp_dir();
        let log = dir.path().join("input");
        let env = [
            ("AGENT_LIST", list.as_str()),
            ("AGENT_INPUT_LOG", log.to_str().unwrap()),
        ];
        let mut client = TestClient::start(LocalProcessLauncher::new(&env), test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a" });
        let (response, _) = client.request(1, "session/list", params).await;
        assert_eq!(response["result"]["sessions"][0]["sessionId"], "a:old");

        let load = json!({ "sessionId": "a:old", "cwd": "/projects/a", "mcpServers": [] });
        let requests = [
            (2, "session/delete", json!({ "sessionId": "a:old" })),
            (3, "session/load", load),
        ];
        for (id, method, params) in requests {
            let (response, _) = client.request(id, method, params).await;
            assert!(response.get("result").is_some(), "{response}");
            assert_eq!(
                agent_got(&log, id)["params"]["sessionId"],
                "old",
                "{method}"
            );
        }
    });
}

/// Test that a client request with a session id of another project, or
/// without a sandbox prefix, is refused. The agent of a project must get
/// only the sessions of its project.
///   1. Load `b:s1` and then `s1` in sandbox `a`
///   2. Check that both get the error of another project
#[test]
fn session_id_of_other_project_is_refused() {
    block_on_local(async {
        let mut client = TestClient::start(LocalProcessLauncher::new(&[]), test_config(true));
        client.initialize(json!({})).await;
        for (id, sid) in [(1, "b:s1"), (2, "s1")] {
            let params = json!({ "sessionId": sid, "cwd": "/projects/a", "mcpServers": [] });
            let (response, _) = client.request(id, "session/load", params).await;
            let message = response["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("not a session of this project"),
                "{response}"
            );
        }
    });
}

/// Test that a load with the id of an open session of another project, and
/// a session dir in this project, keeps the open session. The new agent
/// refuses the id, so the old agent must keep the session.
///   1. Start the session `a:s1` in sandbox `a`
///   2. Load `a:s1` with a session dir in sandbox `b`, and check the error
///   3. Send a prompt to `a:s1` and check that the agent in `a` answers
#[test]
fn load_of_other_project_keeps_open_session() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_SESSION_IDS", "1")]);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        assert_eq!(client.new_session(1, "/projects/a").await, "a:s1");
        let params = json!({ "sessionId": "a:s1", "cwd": "/projects/b", "mcpServers": [] });
        let (response, _) = client.request(2, "session/load", params).await;
        assert!(response.get("error").is_some(), "{response}");

        let (response, others) = client.prompt(3, "a:s1").await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert!(
            others.iter().any(|m| m["params"]["from"] == "a"),
            "{others:?}"
        );
    });
}
