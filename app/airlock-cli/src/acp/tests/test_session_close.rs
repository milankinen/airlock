//! Tests for the close of the ACP sessions of a session, with the fake
//! agent process.

use serde_json::{Value, json};

use crate::acp::rpc::CANCEL_REQUEST;
use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Test that only an OK close of the session that the session request
/// started ends the session. A close of a sub agent session, or a failed
/// close, must not end the main session.
///   1. Start a session with an agent that sends an update in a new session
///      `child`
///   2. Close `child`, and check that a prompt to the main session gets its
///      answer
///   3. Close the main session with an error answer, and check that a
///      prompt to it gets its answer
///   4. Close the main session with an OK answer, and check that the agent
///      stops and a prompt to the session gets an error
#[test]
fn only_ok_close_of_main_session_ends_session() {
    block_on_local(async {
        let say = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"child","update":{}}}"#;
        let launcher = LocalProcessLauncher::new(&[("AGENT_SAY", say)]);
        let launches = launcher.launches.clone();
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        let sid = client.sid.clone();

        // The fake agent refuses a request with a bad cursor.
        let closes = [
            (3, json!({ "sessionId": "child" })),
            (5, json!({ "sessionId": sid, "cursor": "bad" })),
        ];
        for (id, params) in closes {
            client.request(id, "session/close", params).await;
            let (response, _) = client.prompt(id + 1, &sid).await;
            assert_eq!(response["result"], json!({}), "{response}");
        }

        let params = json!({ "sessionId": sid });
        let (response, _) = client.request(7, "session/close", params).await;
        assert_eq!(response["result"], json!({}), "{response}");
        while client.recv().await.pointer("/params/last").is_none() {}
        let (response, _) = client.prompt(8, &sid).await;
        assert!(response.get("error").is_some(), "{response}");
        assert_eq!(launches.get(), 1);
    });
}

/// Test that the close of a session ends the open agent requests of the
/// session, and that the client gets a cancel for each. Else the editor
/// keeps a dialog open that no agent waits for.
///   1. Start a session with an agent that asks for a permission at each
///      prompt, and send a prompt
///   2. Close the session with an OK answer
///   3. Check that the client gets a cancel of the permission request
#[test]
fn close_of_session_cancels_open_agent_requests() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_ASK", "5")]);
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        let sid = client.sid.clone();
        let (_, others) = client.prompt(3, &sid).await;
        let ask = others
            .iter()
            .find(|m| m["method"] == "session/request_permission")
            .unwrap_or_else(|| panic!("{others:?}"))
            .clone();

        let (response, _) = client
            .request(4, "session/close", json!({ "sessionId": sid }))
            .await;
        assert_eq!(response["result"], json!({}), "{response}");
        let cancel: Value = loop {
            let msg = client.recv().await;
            if msg["method"] == CANCEL_REQUEST {
                break msg;
            }
        };
        assert_eq!(cancel["params"]["requestId"], ask["id"], "{cancel}");
    });
}
