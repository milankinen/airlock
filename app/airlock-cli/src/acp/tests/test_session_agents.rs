//! Tests for the own agent process of each session, in one or many
//! sandboxes, with the fake agent process.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Get the agent process id in a session id of the fake agent:
/// `<sandbox>-<pid>-<N>`.
fn agent_pid(sid: &str) -> &str {
    sid.split('-').nth(1).unwrap_or_else(|| panic!("{sid}"))
}

/// Test that each session runs in its own agent process with its own
/// handshake, also in the same sandbox, and that the prompts and the agent
/// requests stay in their session.
///   1. Start two sessions in sandbox `a` and one in sandbox `b`, with an
///      agent that asks for a permission at each prompt
///   2. Check that three agents launched, each with its own process and
///      one `initialize`
///   3. Send a prompt to each session
///   4. Check that each agent got only the prompt of its session, and that
///      each session gets its permission request and its prompt response
#[test]
fn each_session_runs_in_own_agent() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_ASK", "99")]);
        let launched = launcher.launched.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;
        let mut all = Vec::new();
        let mut sids = Vec::new();
        for (id, cwd) in [(1, "/projects/a"), (2, "/projects/a/x"), (3, "/projects/b")] {
            let params = json!({ "cwd": cwd, "mcpServers": [] });
            let (response, others) = client.request(id, "session/new", params).await;
            let sid = response["result"]["sessionId"].as_str().unwrap();
            sids.push(sid.to_string());
            all.extend(others);
        }
        assert_eq!(*launched.borrow(), ["a", "a", "b"]);
        let mut pids: Vec<&str> = sids.iter().map(|s| agent_pid(s)).collect();
        pids.dedup();
        assert_eq!(pids.len(), 3, "{sids:?}");

        for (id, sid) in [(4, &sids[0]), (5, &sids[1]), (6, &sids[2])] {
            let (response, others) = client.prompt(id, sid).await;
            assert!(response.get("result").is_some(), "{response}");
            let prompts: Vec<&Value> = others
                .iter()
                .filter_map(|m| m.pointer("/params/seen"))
                .filter(|m| m["method"] == "session/prompt")
                .collect();
            assert_eq!(prompts.len(), 1, "{others:?}");
            assert_eq!(prompts[0]["id"], id);
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
        assert_eq!(inits, 3);
    });
}

/// Test that the close of a session stops only its agent: the agent input
/// closes and the agent can save its state. The other session of the
/// sandbox keeps its agent, and a new session launches a new agent.
///   1. Start two sessions in sandbox `a`, with an agent that exits with
///      code 5 at the end of its input
///   2. Close the first session and check that its agent writes its last
///      message
///   3. Send a prompt to the second session and check its response
///   4. Start a new session and check that a new agent launches
///   5. Close the client and check the exit code 5
#[test]
fn session_close_stops_only_its_agent() {
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
        while client.recv().await.pointer("/params/last").is_none() {}
        let (response, _) = client.prompt(4, &second).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(launches.get(), 2);

        client.new_session(5, "/projects/a").await;
        assert_eq!(launches.get(), 3);
        assert_eq!(client.exit_code().await, 5);
    });
}

/// Test that an agent exit tells the client about the end of its session,
/// and that the other session of the same sandbox and the proxy keep
/// running. Each session has its own agent. The editor must not wait for
/// answers that never come.
///   1. Start two sessions in sandbox `a`, with and without the session
///      notice feature of the client
///   2. Send a request to the first session that gets no reply, and make
///      its agent exit with code 7
///   3. Check that the request gets an error and only the first session
///      gets a notice or an agent message
///   4. Check that a prompt to the second session gets its answer
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
            let sb = client.new_session(2, "/projects/a").await;

            client
                .send_request(3, "test/hold", json!({ "sessionId": sa }))
                .await;
            client.notify("test/exit", json!({ "sessionId": sa })).await;
            let (response, others) = client.response(3).await;
            assert_eq!(response["error"]["code"], -32603, "{response}");
            let mut others = others;
            others.push(client.recv().await);
            let updates: Vec<&Value> = others
                .iter()
                .filter(|m| m.pointer("/params/update").is_some())
                .collect();
            assert_eq!(updates.len(), 1, "{others:?}");
            let update = updates[0];
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
