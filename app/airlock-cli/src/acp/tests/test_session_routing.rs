//! Tests for the routing of the client messages to the agents of their
//! sessions, with in-memory fake session handles.

use serde_json::{Value, json};

use crate::test_cfg::acp::{FakeSession, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Start the fake proxy, do the handshake and open the sessions `sa` in
/// `/projects/a` and `sb` in `/projects/b`.
/// Returns:
///   The client and the two agents.
async fn two_sessions() -> (TestClient, FakeSession, FakeSession) {
    let (mut client, mut handles) = TestClient::start_fake(test_config(true));
    client.initialize(json!({})).await;
    let mut opened = Vec::new();
    for (id, cwd, sid) in [(1, "/projects/a", "sa"), (2, "/projects/b", "sb")] {
        client
            .send_request(id, "session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await;
        let handle = handles.recv().await.expect("agent");
        assert_eq!(handle.cwd, cwd);
        handle.open_session(sid).await;
        let (response, _) = client.response(id).await;
        assert_eq!(response["result"]["sessionId"], sid);
        opened.push(handle);
    }
    let b = opened.pop().unwrap();
    let a = opened.pop().unwrap();
    (client, a, b)
}

/// Make a session notification of `session`.
fn update(session: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": "session/update",
            "params": { "sessionId": session, "update": {} } })
}

/// Test that each session request starts its own agent and that the client
/// messages go to the agent of their session. Agents in many sandboxes
/// serve one client at the same time.
///   1. Open sessions `sa` and `sb` in two agents
///   2. Send a prompt to each session and a cancel to `sb`
///   3. Check that each agent got only the messages of its session
///   4. Check that the agent answers reach the client
///   5. Send a prompt to an unknown session and check the error
#[test]
fn messages_route_to_agent_of_their_session() {
    block_on_local(async {
        let (mut client, mut a, mut b) = two_sessions().await;
        assert_ne!(a.id, b.id);

        client.send_prompt(10, "sa").await;
        client.send_prompt(11, "sb").await;
        client
            .notify("session/cancel", json!({ "sessionId": "sb" }))
            .await;
        assert_eq!(a.recv().await["id"], 10);
        assert_eq!(b.recv().await["id"], 11);
        assert_eq!(b.recv().await["method"], "session/cancel");

        b.say(json!({ "jsonrpc": "2.0", "id": 11, "result": {} }))
            .await;
        a.say(json!({ "jsonrpc": "2.0", "id": 10, "result": {} }))
            .await;
        assert!(client.response(11).await.0.get("result").is_some());
        assert!(client.response(10).await.0.get("result").is_some());

        let (response, _) = client.prompt(12, "nope").await;
        assert_eq!(response["error"]["code"], -32602);
    });
}

/// Test that an agent cannot send messages in the session of another
/// agent, and that a new session id of an agent becomes its own. Sub
/// agents make child sessions that the client did not open.
///   1. Open sessions `sa` and `sb` in two agents
///   2. Make agent `b` send an update in `sa`, and then in a new session
///      `child`
///   3. Check that the client got only the `child` update
///   4. Send a prompt to `child` and check that agent `b` gets it
#[test]
fn agent_messages_stay_in_own_sessions() {
    block_on_local(async {
        let (mut client, _a, mut b) = two_sessions().await;
        b.say(update("sa")).await;
        b.say(update("child")).await;
        let msg = client.recv().await;
        assert_eq!(msg["params"]["sessionId"], "child", "{msg}");

        client.send_prompt(5, "child").await;
        assert_eq!(b.recv().await["id"], 5);
    });
}

/// Test that the requests without a session get a reply from the proxy
/// and start no agent. Each agent knows only its own sessions, and the
/// proxy shows no login methods.
///   1. Send `session/list` without a session and check the empty list
///   2. Send `authenticate` and `logout`, and check `METHOD_NOT_FOUND`
///   3. Check that no agent started
#[test]
fn requests_without_session_are_answered_by_proxy() {
    block_on_local(async {
        let (mut client, mut handles) = TestClient::start_fake(test_config(true));
        client.initialize(json!({})).await;
        let (response, _) = client.request(1, "session/list", json!({})).await;
        assert_eq!(response["result"], json!({ "sessions": [] }));
        let params = json!({ "methodId": "x" });
        let (response, _) = client.request(2, "authenticate", params).await;
        assert_eq!(response["error"]["code"], -32601);
        let (response, _) = client.request(3, "logout", json!({})).await;
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(client.exit_code().await, 0);
        assert!(handles.recv().await.is_none());
    });
}

/// Test that a session list and a delete of a listed session start agents
/// in the dir of the list, and that a delete of a session that is in no
/// list is refused. The editor deletes sessions from the list, and the
/// sessions have no agent then.
///   1. Send `session/list` in `/projects/a` and check that an agent in
///      that dir gets it
///   2. Make the agent keep the session `old`, reply with it and exit.
///      Check that the client gets the rows and the proxy stops the agent
///   3. Send `session/delete` for `old` and check that a new agent in
///      `/projects/a` gets it. Make it forget `old`, reply and exit
///   4. Send the delete again, and a delete of an unknown session, and
///      check the unknown session errors
#[test]
fn delete_of_listed_session_runs_in_agent_of_list_dir() {
    block_on_local(async {
        let (mut client, mut handles) = TestClient::start_fake(test_config(true));
        client.initialize(json!({})).await;
        let rows = json!({ "sessions": [{ "sessionId": "old", "cwd": "/projects/a" }] });

        client
            .send_request(1, "session/list", json!({ "cwd": "/projects/a" }))
            .await;
        let mut lister = handles.recv().await.expect("agent");
        assert_eq!(lister.cwd, "/projects/a");
        assert_eq!(lister.first["method"], "session/list");
        lister.set_session_dir("old", Some("/projects/a"));
        lister
            .say(json!({ "jsonrpc": "2.0", "id": 1, "result": rows }))
            .await;
        lister.exit(0).await;
        let (response, _) = client.response(1).await;
        assert_eq!(response["result"], rows);
        lister.stopped().await;

        let params = json!({ "sessionId": "old" });
        client
            .send_request(2, "session/delete", params.clone())
            .await;
        let mut deleter = handles.recv().await.expect("agent");
        assert_eq!(deleter.cwd, "/projects/a");
        assert_eq!(deleter.first["method"], "session/delete");
        deleter.set_session_dir("old", None);
        deleter
            .say(json!({ "jsonrpc": "2.0", "id": 2, "result": {} }))
            .await;
        deleter.exit(0).await;
        let (response, _) = client.response(2).await;
        assert_eq!(response["result"], json!({}));
        deleter.stopped().await;

        let (response, _) = client.request(3, "session/delete", params).await;
        assert_eq!(response["error"]["code"], -32602);
        let params = json!({ "sessionId": "nope" });
        let (response, _) = client.request(4, "session/delete", params).await;
        assert_eq!(response["error"]["code"], -32602);
    });
}

/// Test that an agent request in the session of another agent gets an
/// error. The agent must not wait for an answer that never comes.
///   1. Open sessions `sa` and `sb` in two agents
///   2. Make agent `b` send a permission request in `sa`
///   3. Check that agent `b` gets an error response for the request
#[test]
fn agent_request_for_other_session_gets_error() {
    block_on_local(async {
        let (_client, _a, mut b) = two_sessions().await;
        let params = json!({ "sessionId": "sa", "options": [], "toolCall": {} });
        b.say(json!({ "jsonrpc": "2.0", "id": 7,
                      "method": "session/request_permission", "params": params }))
            .await;
        let answer = b.recv().await;
        assert_eq!(answer["id"], 7, "{answer}");
        assert_eq!(answer["error"]["code"], -32602, "{answer}");
    });
}
