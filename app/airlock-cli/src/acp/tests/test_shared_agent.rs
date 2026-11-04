//! Tests for one agent process that serves all sessions in one sandbox,
//! with the fake agent process.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that two sessions in the same sandbox share one agent process with
/// one handshake, and that the agent messages go to the correct session.
/// One agent process for each sandbox uses less memory.
///   1. Start two sessions in sandbox `a`, with an agent that asks for a
///      permission at each prompt
///   2. Check that one agent launched and got one `initialize`
///   3. Send a prompt to each session
///   4. Check that each session gets its permission request and its
///      prompt response
#[test]
fn sessions_in_same_sandbox_share_one_agent() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_ASK", "99")]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let mut all = Vec::new();
        let mut sids = Vec::new();
        for (id, cwd) in [(1, "/projects/a"), (2, "/projects/a/x")] {
            let params = json!({ "cwd": cwd, "mcpServers": [] });
            let (response, others) = client.request(id, "session/new", params).await;
            sids.push(
                response["result"]["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
            all.extend(others);
        }
        assert_eq!(launches.get(), 1);

        for (id, sid) in [(3, &sids[0]), (4, &sids[1])] {
            let (response, others) = client.prompt(id, sid).await;
            assert!(response.get("result").is_some(), "{response}");
            let ask = others
                .iter()
                .find(|m| m["method"] == "session/request_permission")
                .unwrap_or_else(|| panic!("{others:?}"));
            assert_eq!(ask["params"]["sessionId"], json!(sid));
            all.extend(others);
        }
        let inits = seen(&all)
            .iter()
            .filter(|m| m["method"] == "initialize")
            .count();
        assert_eq!(inits, 1);
    });
}

/// Test that the close of one session keeps the shared agent for the other
/// session, and that the close of the last session stops the agent. An
/// agent without sessions must not stay in the sandbox.
///   1. Start two sessions in sandbox `a`, with an agent that exits with
///      code 5 at the end of its input
///   2. Close the first session and check that a prompt to the second
///      session gets its response
///   3. Close the second session and check that the agent writes its last
///      message
///   4. Start a new session and check that a new agent launches
///   5. Close the client and check the exit code 5
#[test]
fn close_of_last_session_stops_shared_agent() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_EOF", "5")]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let first = client.new_session(1, "/projects/a").await;
        let second = client.new_session(2, "/projects/a").await;

        let (response, _) = client
            .request(3, "session/close", json!({ "sessionId": first }))
            .await;
        assert!(response.get("result").is_some(), "{response}");
        let (response, _) = client.prompt(4, &second).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(launches.get(), 1);

        let (response, _) = client
            .request(5, "session/close", json!({ "sessionId": second }))
            .await;
        assert!(response.get("result").is_some(), "{response}");
        while client.recv().await.pointer("/params/last").is_none() {}

        client.new_session(6, "/projects/a").await;
        assert_eq!(launches.get(), 2);
        assert_eq!(client.exit_code().await, 5);
    });
}

/// Test that the exit of a shared agent ends all its sessions with an
/// error for each open request and a session notice, and that a new
/// session launches a new agent. The editor must not wait for answers that
/// never come.
///   1. Start two sessions in sandbox `a`, with the session notice feature
///      of the client
///   2. Send a request without a reply to each session
///   3. Make the agent exit with code 7
///   4. Check that both requests get an error and both sessions get a
///      notice
///   5. Start a new session and check that a new agent launches
///   6. Close the client and check the exit code 7
#[test]
fn shared_agent_exit_ends_all_its_sessions() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client
            .initialize(json!({ "session": { "notices": {} } }))
            .await;
        let sids = [
            client.new_session(1, "/projects/a").await,
            client.new_session(2, "/projects/a").await,
        ];
        for (id, sid) in [(3, &sids[0]), (4, &sids[1])] {
            client
                .send_request(id, "test/hold", json!({ "sessionId": sid }))
                .await;
        }
        client
            .notify("test/exit", json!({ "sessionId": sids[0] }))
            .await;

        let mut errors = Vec::new();
        let mut notices = Vec::new();
        while errors.len() < 2 || notices.len() < 2 {
            let msg: Value = client.recv().await;
            if msg.get("error").is_some() {
                errors.push(msg["id"].clone());
            } else if msg.pointer("/params/update/sessionUpdate") == Some(&json!("notice")) {
                notices.push(msg["params"]["sessionId"].clone());
            }
        }
        errors.sort_by_key(Value::as_i64);
        assert_eq!(errors, [json!(3), json!(4)]);
        notices.sort_by_key(Value::to_string);
        let mut expected = sids.map(|sid| json!(sid));
        expected.sort_by_key(Value::to_string);
        assert_eq!(notices, expected);

        client.new_session(5, "/projects/a").await;
        assert_eq!(launches.get(), 2);
        assert_eq!(client.exit_code().await, 7);
    });
}

/// Test that a session list in a sandbox with an open session uses the
/// agent of that session, and that the session stays open. A list must
/// not launch a second agent or end the work of the user.
///   1. Start a session in sandbox `a`
///   2. Send `session/list` in the sandbox and check the host path rows
///   3. Check that one agent launched and that a prompt to the session
///      gets its response
#[test]
fn session_list_reuses_agent_of_open_session() {
    block_on_local(async {
        let list = json!({ "sessions": [{ "sessionId": "x", "cwd": "/work" }] });
        let launcher = LocalProcessLauncher::new(&[("AGENT_LIST", &list.to_string())]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let sid = client.new_session(1, "/projects/a").await;

        let params = json!({ "cwd": "/projects/a" });
        let (response, _) = client.request(2, "session/list", params).await;
        assert_eq!(response["result"]["sessions"][0]["cwd"], "/projects/a");
        let (response, _) = client.prompt(3, &sid).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(launches.get(), 1);
    });
}
