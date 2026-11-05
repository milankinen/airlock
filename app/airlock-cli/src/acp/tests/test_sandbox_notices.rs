//! Tests for the notices of a sandbox in the sessions of its agent.

use serde_json::{Value, json};

use crate::messages::acp::SANDBOX_NOTICE;
use crate::sandbox::NoticeLog;
use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Make a notice log with the texts `texts`.
fn log(texts: &[&str]) -> NoticeLog {
    let log = NoticeLog::new();
    for text in texts {
        log.push((*text).to_string());
    }
    log
}

/// Check if `msg` is a session notice or agent text of any session.
fn is_notice(msg: &Value) -> bool {
    let update = msg.pointer("/params/update/sessionUpdate");
    update == Some(&json!("notice")) || update == Some(&json!("agent_message_chunk"))
}

/// Check if `msg` is a session notice or agent text of `sid`.
fn is_session_message(msg: &Value, sid: &str) -> bool {
    let update = msg.pointer("/params/update/sessionUpdate");
    msg["method"] == "session/update"
        && msg["params"]["sessionId"] == sid
        && (update == Some(&json!("notice")) || update == Some(&json!("agent_message_chunk")))
}

/// Get the next `count` messages from the proxy. Check that each is a
/// message of `sid`.
/// Returns:
///   The updates of the messages.
// The fake agent sends nothing after its `session/new` response. A real
// agent can send messages before the warnings.
async fn session_messages(client: &mut TestClient, sid: &str, count: usize) -> Vec<Value> {
    let mut got = Vec::new();
    for _ in 0..count {
        let msg = client.recv().await;
        assert!(is_session_message(&msg, sid), "{msg}");
        got.push(msg["params"]["update"].clone());
    }
    got
}

/// Make the session notice of the sandbox notice `text`.
fn warning(text: &str) -> Value {
    json!({
        "sessionUpdate": "notice", "severity": "warning",
        "title": SANDBOX_NOTICE, "description": text,
    })
}

/// Test that each session shows all notices of the sandbox after the
/// response to its session request. The editor user must see them in each
/// session, also the notices that came after the agent launch.
///   1. Use a sandbox with two notices
///   2. Do the client handshake with the session notice feature
///   3. Start a session and check that the two warnings follow the response
///   4. Add a third notice to the sandbox
///   5. Start a second session in the same agent and check that it gets all
///      three warnings after its response
///   6. Send a prompt to the first session and check that it got no more
///      warnings
#[test]
fn each_session_shows_all_sandbox_notices_after_response() {
    block_on_local(async {
        let notices = log(&["config changed", "browser request refused"]);
        let launcher = LocalProcessLauncher::new(&[]).with_notices(&notices);
        let launches = launcher.launches.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client
            .initialize(json!({ "session": { "notices": {} } }))
            .await;

        let first = client.new_session(1, "/projects/a").await;
        let got = session_messages(&mut client, &first, 2).await;
        let expected = [
            warning("config changed"),
            warning("browser request refused"),
        ];
        assert_eq!(got, expected);

        notices.push("later notice".into());
        let second = client.new_session(2, "/projects/a").await;
        let got = session_messages(&mut client, &second, 3).await;
        let expected = [
            warning("config changed"),
            warning("browser request refused"),
            warning("later notice"),
        ];
        assert_eq!(got, expected);
        assert_eq!(launches.get(), 1);

        let (response, others) = client.prompt(3, &first).await;
        assert!(response.get("result").is_some(), "{response}");
        let more: Vec<_> = others
            .iter()
            .filter(|m| is_session_message(m, &first))
            .collect();
        assert!(more.is_empty(), "{more:?}");
    });
}

/// Test that a client without the session notice feature gets each
/// sandbox notice as agent text. Such a client does not show notices, and
/// the user must still see them.
///   1. Use a sandbox with one notice
///   2. Do the client handshake without the session notice feature
///   3. Start a session and check that an agent text chunk with the title
///      and the text follows the response
#[test]
fn sandbox_notice_without_notice_feature_is_agent_text() {
    block_on_local(async {
        let notices = log(&["config changed"]);
        let launcher = LocalProcessLauncher::new(&[]).with_notices(&notices);
        let mut client = TestClient::start(launcher, test_config(true));
        client.initialize(json!({})).await;

        let sid = client.new_session(1, "/projects/a").await;
        let got = session_messages(&mut client, &sid, 1).await;
        let text = format!("{SANDBOX_NOTICE}: config changed");
        let expected = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        });
        assert_eq!(got, [expected]);
    });
}

/// Test that a sandbox without notices gives no warning. The session must
/// not show empty messages.
///   1. Use a sandbox with an empty notice log
///   2. Start a session and send a prompt to it
///   3. Check that the session got no notice
#[test]
fn sandbox_without_notices_gives_no_warning() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]).with_notices(&log(&[]));
        let mut client = TestClient::start(launcher, test_config(true));
        client
            .initialize(json!({ "session": { "notices": {} } }))
            .await;

        let sid = client.new_session(1, "/projects/a").await;
        let (response, others) = client.prompt(2, &sid).await;
        assert!(response.get("result").is_some(), "{response}");
        let notices: Vec<_> = others
            .iter()
            .filter(|m| is_session_message(m, &sid))
            .collect();
        assert!(notices.is_empty(), "{notices:?}");
    });
}

/// Test that a slow notice read of one session does not stop the other
/// sessions of the agent. The read goes to the sandbox and can take time.
///   1. Use a sandbox with one notice whose first notice read waits
///   2. Start a session: its notice read waits
///   3. Start a second session and check that it gets its response and
///      its warning, and that the first session got no warning yet
///   4. Let the first read continue and check that the first session gets
///      its warning
#[test]
fn slow_notice_read_does_not_stop_other_session() {
    block_on_local(async {
        let notices = log(&["config changed"]);
        let (launcher, open) = LocalProcessLauncher::new(&[]).with_held_notices(&notices);
        let mut client = TestClient::start(launcher, test_config(true));
        client
            .initialize(json!({ "session": { "notices": {} } }))
            .await;

        let first = client.new_session(1, "/projects/a").await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, others) = client.request(2, "session/new", params).await;
        let second = response["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!others.iter().any(is_notice), "{others:?}");
        let got = session_messages(&mut client, &second, 1).await;
        assert_eq!(got, [warning("config changed")]);

        open.send(()).unwrap();
        let got = session_messages(&mut client, &first, 1).await;
        assert_eq!(got, [warning("config changed")]);
    });
}

/// Test that a failed session request gives no warnings. The client does
/// not know a session to show them in.
///   1. Use a sandbox with one notice and an agent that fails all requests
///   2. Send `session/new` and check the error
///   3. Send a second `session/new` and check that no warning came before
///      its error
#[test]
fn failed_session_request_gives_no_warning() {
    block_on_local(async {
        let error = r#"{"code":-32000,"message":"no session"}"#;
        let launcher = LocalProcessLauncher::new(&[("AGENT_ERROR", error)])
            .with_notices(&log(&["config changed"]));
        let mut client = TestClient::start(launcher, test_config(true));
        client
            .initialize(json!({ "session": { "notices": {} } }))
            .await;

        let (code, _) = client.session_error(1, "/projects/a").await;
        assert_eq!(code, -32000);
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, others) = client.request(2, "session/new", params).await;
        assert_eq!(response["error"]["code"], -32000, "{response}");
        assert!(!others.iter().any(is_notice), "{others:?}");
    });
}
