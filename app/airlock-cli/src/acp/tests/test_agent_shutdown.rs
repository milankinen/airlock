//! Tests for the stop of single agents at session close and load, with
//! in-memory fake session handles.

use serde_json::json;
use tokio::sync::mpsc;

use crate::test_cfg::acp::{FakeSession, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Start the fake proxy, do the handshake and open one session for each
/// id in `sessions`, each in its own agent.
/// Returns:
///   The client, the receiver of later agents and the agents.
async fn open(
    sessions: &[&str],
) -> (
    TestClient,
    mpsc::UnboundedReceiver<FakeSession>,
    Vec<FakeSession>,
) {
    let (mut client, mut handles) = TestClient::start_fake(test_config(true));
    client.initialize(json!({})).await;
    let mut opened = Vec::new();
    for (id, sid) in (1..).zip(sessions) {
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        client.send_request(id, "session/new", params).await;
        let handle = handles.recv().await.unwrap();
        handle.open_session(sid).await;
        client.response(id).await;
        opened.push(handle);
    }
    (client, handles, opened)
}

/// Test that a session close stops the agent of the session after its
/// answer. The agent ran only for that session.
///   1. Open sessions `sa` and `sb`
///   2. Close `sa` and make its agent answer with an error
///   3. Check that the client gets the answer and the agent stops
///   4. Check that a prompt to `sa` gets the unknown session error, and a
///      prompt to `sb` reaches its agent
#[test]
fn session_close_stops_agent_of_session() {
    block_on_local(async {
        let (mut client, _handles, mut opened) = open(&["sa", "sb"]).await;
        let params = json!({ "sessionId": "sa" });
        client.send_request(10, "session/close", params).await;
        assert_eq!(opened[0].recv().await["method"], "session/close");
        let error = json!({ "code": -32603, "message": "no" });
        opened[0]
            .say(json!({ "jsonrpc": "2.0", "id": 10, "error": error }))
            .await;
        let (response, _) = client.response(10).await;
        assert_eq!(response["error"]["message"], "no");
        opened[0].stopped().await;

        let (response, _) = client.prompt(11, "sa").await;
        assert_eq!(response["error"]["code"], -32602);
        client.send_prompt(12, "sb").await;
        assert_eq!(opened[1].recv().await["id"], 12);
    });
}

/// Test that the last output of the old agent of a loaded session does not
/// take the session back. The new agent replays the session before its
/// load response, and the client must get the replay.
///   1. Open session `sa`
///   2. Load `sa` again and wait until the old agent stops
///   3. Make the old agent send an update in `sa`, and then the new agent
///      send a replay update in `sa` and its load response
///   4. Check that the client gets both updates and an OK response
#[test]
fn old_agent_output_does_not_take_back_loaded_session() {
    block_on_local(async {
        let (mut client, mut handles, opened) = open(&["sa"]).await;
        let params = json!({ "sessionId": "sa", "cwd": "/projects/a", "mcpServers": [] });
        client.send_request(5, "session/load", params).await;
        opened[0].stopped().await;
        let new = handles.recv().await.unwrap();
        opened[0].say(update("sa", "old")).await;
        new.say(update("sa", "new")).await;
        new.open_session("sa").await;
        let (response, others) = client.response(5).await;
        assert!(response.get("result").is_some(), "{response}");
        assert_eq!(update_sources(&others), ["old", "new"]);
    });
}

/// Test that an agent that stops still gets its last output in its own
/// session to the client, but not in a new session. The end of a prompt
/// must not get lost at the client end.
///   1. Open session `sa` with an agent that does not end at its stop
///   2. Send a prompt, then close the client and wait for the stop
///   3. Make the agent send an update in `sa`, an update in a new session
///      and the prompt response
///   4. Check that the client gets only the `sa` update and the response
#[test]
fn stopping_agent_output_reaches_client_only_in_own_session() {
    block_on_local(async {
        let (mut client, _handles, mut opened) = open(&["sa"]).await;
        opened[0].set_linger();
        client.send_prompt(7, "sa").await;
        assert_eq!(opened[0].recv().await["id"], 7);
        client.close();
        opened[0].stopped().await;
        opened[0].say(update("sa", "last")).await;
        opened[0].say(update("child", "child")).await;
        opened[0]
            .say(json!({ "jsonrpc": "2.0", "id": 7, "result": {} }))
            .await;
        let (_, others) = client.response(7).await;
        assert_eq!(update_sources(&others), ["last"]);
        opened[0].exit(0).await;
        assert_eq!(client.proxy_end().await, 0);
    });
}

/// Make a `session/update` in the session `sid` with the marker `from`.
fn update(sid: &str, from: &str) -> serde_json::Value {
    let params = json!({ "sessionId": sid, "update": { "from": from } });
    json!({ "jsonrpc": "2.0", "method": "session/update", "params": params })
}

/// Get the markers of the `session/update` messages in `msgs`.
fn update_sources(msgs: &[serde_json::Value]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| m.pointer("/params/update/from")?.as_str())
        .map(str::to_string)
        .collect()
}

/// Test that a load of an open session stops its old agent and starts a
/// new one. Two agents must not write to one session.
///   1. Open session `sa`
///   2. Load `sa` again
///   3. Check that the old agent stops and a new agent gets the load
///   4. Check that a prompt to `sa` reaches the new agent
#[test]
fn load_of_open_session_stops_old_agent() {
    block_on_local(async {
        let (mut client, mut handles, opened) = open(&["sa"]).await;
        let params = json!({ "sessionId": "sa", "cwd": "/projects/a", "mcpServers": [] });
        client.send_request(5, "session/load", params).await;
        opened[0].stopped().await;
        let mut new = handles.recv().await.unwrap();
        assert_eq!(new.first["method"], "session/load");
        new.open_session("sa").await;
        client.response(5).await;

        client.send_prompt(6, "sa").await;
        assert_eq!(new.recv().await["id"], 6);
    });
}

/// Test that the client end waits for all agents, also for an agent that
/// started to stop before, and gives the first exit code that is not 0.
///   1. Open sessions `sa` and `sb`, with an agent for `sa` that does not
///      end at its stop
///   2. Close `sa`, and then close the client
///   3. Check that the proxy still runs
///   4. Make the agent of `sa` exit with code 9 and check the exit code 9
#[test]
fn client_end_waits_for_agents_that_stop() {
    block_on_local(async {
        let (mut client, _handles, mut opened) = open(&["sa", "sb"]).await;
        opened[0].set_linger();
        client
            .send_request(10, "session/close", json!({ "sessionId": "sa" }))
            .await;
        opened[0].recv().await;
        opened[0]
            .say(json!({ "jsonrpc": "2.0", "id": 10, "result": {} }))
            .await;
        client.response(10).await;
        opened[0].stopped().await;

        client.close();
        opened[1].stopped().await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(client.proxy_runs());
        opened[0].exit(9).await;
        assert_eq!(client.proxy_end().await, 9);
    });
}
