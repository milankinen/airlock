//! Tests for the ids of the agent requests, with in-memory fake session handles.

use serde_json::json;

use crate::test_cfg::acp::{TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that two agents can use the same request id. The client must see
/// different ids, and each answer must reach its agent with the original
/// id. Else an answer goes to the wrong agent.
///   1. Open sessions `sa` and `sb` in two agents
///   2. Make both agents send `session/request_permission` with the id 1
///   3. Check that the client got two different ids
///   4. Answer both requests and check that each agent got its answer with
///      the id 1
///   5. Answer one id again and check that no agent gets it
#[test]
fn same_request_id_of_two_agents_is_kept_apart() {
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
        for (handle, sid) in opened.iter().zip(["sa", "sb"]) {
            let params = json!({ "sessionId": sid, "options": [], "toolCall": {} });
            let msg = json!({ "jsonrpc": "2.0", "id": 1,
                              "method": "session/request_permission", "params": params });
            handle.say(msg).await;
        }
        let first = client.recv().await;
        let second = client.recv().await;
        assert_eq!(first["params"]["sessionId"], "sa");
        assert_eq!(second["params"]["sessionId"], "sb");
        assert_ne!(first["id"], second["id"]);

        for (msg, answer) in [(&second, "b"), (&first, "a")] {
            let result = json!({ "outcome": { "outcome": "selected", "optionId": answer } });
            client
                .send(&json!({ "jsonrpc": "2.0", "id": msg["id"], "result": result }))
                .await;
        }
        for (handle, answer) in opened.iter_mut().zip(["a", "b"]) {
            let got = handle.recv().await;
            assert_eq!(got["id"], 1);
            assert_eq!(got["result"]["outcome"]["optionId"], answer);
        }

        client
            .send(&json!({ "jsonrpc": "2.0", "id": first["id"], "result": {} }))
            .await;
        client.send_prompt(3, "sa").await;
        assert_eq!(opened[0].recv().await["id"], 3);
    });
}
