//! Tests for the agent exit and the proxy end with many agents, with
//! in-memory fake session handles.

use serde_json::json;

use crate::test_cfg::acp::{TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that one agent exit ends only the session of that agent. The
/// proxy keeps running for the other sessions and for new sessions, and
/// gives the exit code of the agent at the end.
///   1. Open sessions `sa` and `sb` in two agents
///   2. Make agent `a` exit with code 3
///   3. Check that a prompt to `sa` gets an error and a prompt to `sb`
///      reaches agent `b`
///   4. Start a new session and check that a new agent starts
///   5. Close the client and check the exit code 3
#[test]
fn one_agent_exit_ends_only_its_sessions() {
    block_on_local(async {
        let (mut client, mut handles) = TestClient::start_fake(test_config(true));
        client.initialize(json!({})).await;
        let mut opened = Vec::new();
        for (id, cwd, sid) in [(1, "/projects/a", "sa"), (2, "/projects/b", "sb")] {
            let params = json!({ "cwd": cwd, "mcpServers": [] });
            client.send_request(id, "session/new", params).await;
            let handle = handles.recv().await.unwrap();
            handle.open_session(sid).await;
            client.response(id).await;
            opened.push(handle);
        }
        opened[0].exit(3).await;

        // The proxy forgets the session at the exit message. A prompt
        // before it finds the agent stopped.
        let (response, _) = client.prompt(3, "sa").await;
        let code = response["error"]["code"].as_i64();
        assert!(matches!(code, Some(-32602 | -32603)), "{response}");
        client.send_prompt(4, "sb").await;
        assert_eq!(opened[1].recv().await["id"], 4);

        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        client.send_request(5, "session/new", params).await;
        assert_eq!(handles.recv().await.unwrap().id, 2);
        assert_eq!(client.exit_code().await, 3);
    });
}

/// Test that the client end stops all agents and that the proxy gives the
/// first exit code that is not 0. The editor sees a failure of any agent.
///   1. Open three sessions
///   2. Give the agents the stop codes 0, 4 and 6
///   3. Close the client and check the exit code 4
#[test]
fn client_end_stops_all_agents_with_first_failure_code() {
    block_on_local(async {
        let (mut client, mut handles) = TestClient::start_fake(test_config(true));
        client.initialize(json!({})).await;
        let mut opened = Vec::new();
        for (id, code) in [(1, 0), (2, 4), (3, 6)] {
            let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
            client.send_request(id, "session/new", params).await;
            let handle = handles.recv().await.unwrap();
            handle.set_stop_code(code);
            handle.open_session(&format!("s{id}")).await;
            client.response(id).await;
            opened.push(handle);
        }
        assert_eq!(client.exit_code().await, 4);
    });
}
