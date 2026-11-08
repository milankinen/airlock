//! Tests for the reopen of a session after its agent exited.

use std::time::Duration;

use serde_json::{Value, json};

use crate::acp::Config;
use crate::acp::rpc::CANCEL_REQUEST;
use crate::messages::acp::{REOPEN_NOT_SUPPORTED, REOPEN_QUEUE_FULL, reopen_timeout};
use crate::sandbox::NoticeLog;
use crate::test_cfg::acp::{
    LocalProcessLauncher, TestClient, capture_logs, full_agent_init, test_config,
};
use crate::test_cfg::{block_on_local, temp_dir};

/// Start a session with an agent that gets `env`, and make the agent exit.
/// Returns:
///   The client after the agent stop message, and the number of agent
///   launches.
async fn session_after_agent_exit(
    env: &[(&str, &str)],
) -> (TestClient, std::rc::Rc<std::cell::Cell<usize>>) {
    session_after_agent_exit_with(env, test_config(true)).await
}

/// [`session_after_agent_exit`] with the proxy settings `config`.
async fn session_after_agent_exit_with(
    env: &[(&str, &str)],
    config: Config,
) -> (TestClient, std::rc::Rc<std::cell::Cell<usize>>) {
    session_after_agent_exit_in(LocalProcessLauncher::new(env), config).await
}

/// [`session_after_agent_exit`] with the agent launcher `launcher` and the
/// proxy settings `config`.
async fn session_after_agent_exit_in(
    launcher: LocalProcessLauncher,
    config: Config,
) -> (TestClient, std::rc::Rc<std::cell::Cell<usize>>) {
    let launches = launcher.launches.clone();
    let mut client = TestClient::session(launcher, config, "/projects/a").await;
    client
        .send_in_session(json!({ "jsonrpc": "2.0", "method": "test/exit", "params": {} }))
        .await;
    // The agent stop message comes after the last agent output.
    loop {
        let msg = client.recv().await;
        let text = msg
            .pointer("/params/update/content/text")
            .and_then(Value::as_str);
        if text.is_some_and(|t| t.starts_with("The agent stopped")) {
            break;
        }
    }
    (client, launches)
}

/// Get the methods of the agent input lines in the `seen` messages.
fn seen_methods(messages: &[Value]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|m| m.pointer("/params/seen/method").and_then(Value::as_str))
        .collect()
}

/// Check that the client got no response other than `response`. The
/// response to the reopen request of the proxy must not get to the client.
fn assert_no_other_response(others: &[Value]) {
    assert!(
        others.iter().all(|m| m.get("method").is_some()),
        "{others:?}"
    );
}

/// Test that a prompt to a session whose agent exited resumes the session
/// in a new agent. The editor does not know about the exit, for example at
/// `airlock stop`. Without the resume, the new agent does not know the
/// session and refuses the prompt.
///   1. Start a session with an agent that can resume sessions
///   2. Make the agent exit
///   3. Send a prompt to the same session
///   4. Check that a new agent got `session/resume` before the prompt
///   5. Check that the client got only the prompt response
#[test]
fn prompt_after_agent_exit_resumes_session_in_new_agent() {
    block_on_local(async {
        let (mut client, launches) = session_after_agent_exit(&[]).await;
        let sid = client.sid.clone();
        let (response, others) = client.prompt(3, &sid).await;
        assert_eq!(launches.get(), 2);
        assert_eq!(response["result"], json!({}), "{response}");
        assert_eq!(
            seen_methods(&others),
            ["initialize", "session/resume", "session/prompt"]
        );
        assert_no_other_response(&others);
    });
}

/// Test that the session opens again with a load if the agent cannot
/// resume, and that the client does not get the session history of the
/// load. The client has the history already.
///   1. Start a session with an agent without resume that sends history
///      at a load
///   2. Make the agent exit
///   3. Send a prompt to the same session
///   4. Check that a new agent got `session/load` before the prompt
///   5. Check that the client got no history and only the prompt response
#[test]
fn prompt_after_agent_exit_loads_session_without_history() {
    block_on_local(async {
        let mut init = full_agent_init();
        init["agentCapabilities"]["sessionCapabilities"]
            .as_object_mut()
            .unwrap()
            .remove("resume");
        let init = init.to_string();
        let env = [("AGENT_INIT", init.as_str()), ("AGENT_REPLAY", "1")];
        let (mut client, launches) = session_after_agent_exit(&env).await;
        let sid = client.sid.clone();
        let (response, others) = client.prompt(3, &sid).await;
        assert_eq!(launches.get(), 2);
        assert_eq!(response["result"], json!({}), "{response}");
        assert_eq!(
            seen_methods(&others),
            ["initialize", "session/load", "session/prompt"]
        );
        assert!(
            others
                .iter()
                .all(|m| m.pointer("/params/update/replay").is_none()),
            "{others:?}"
        );
        assert_no_other_response(&others);
    });
}

/// Test that a client cancel of a prompt that waits for the reopen goes to
/// the new agent after the prompt. Else the cancel is lost and the prompt
/// runs.
///   1. Start a session and make its agent exit
///   2. Send a prompt to the session and a cancel of the prompt at once
///   3. Check that the new agent gets the reopen, the prompt and then the
///      cancel
#[test]
fn cancel_of_prompt_that_waits_for_reopen_reaches_new_agent() {
    block_on_local(async {
        let (mut client, _) = session_after_agent_exit(&[]).await;
        let sid = client.sid.clone();
        client.send_prompt(3, &sid).await;
        client
            .notify(CANCEL_REQUEST, json!({ "requestId": 3 }))
            .await;
        let mut seen = Vec::new();
        while seen.last().map(String::as_str) != Some(CANCEL_REQUEST) {
            let msg = client.recv().await;
            if let Some(method) = msg.pointer("/params/seen/method").and_then(Value::as_str) {
                seen.push(method.to_string());
            }
        }
        assert_eq!(
            seen,
            [
                "initialize",
                "session/resume",
                "session/prompt",
                CANCEL_REQUEST
            ]
        );
    });
}

/// Test that a failed reopen gives one error to the waiting prompt, and
/// that the next prompt tries the reopen again. A sandbox that does not
/// start one time must not end the session for good.
///   1. Start a session with an agent that refuses the first reopen
///   2. Make the agent exit
///   3. Send a prompt and check that it gets one error
///   4. Send a second prompt and check that a third agent resumes the
///      session and answers the prompt
#[test]
fn failed_reopen_gives_error_and_next_request_tries_again() {
    block_on_local(async {
        let dir = temp_dir();
        let flag = dir.path().join("fail");
        std::fs::write(&flag, "").unwrap();
        let flag = flag.to_str().unwrap();
        let (mut client, launches) = session_after_agent_exit(&[("AGENT_REOPEN_FAIL", flag)]).await;
        let sid = client.sid.clone();

        let (response, others) = client.prompt(3, &sid).await;
        assert_eq!(response["error"]["code"], -32603, "{response}");
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("Resource not found"), "{message}");
        assert_no_other_response(&others);

        let (response, others) = client.prompt(4, &sid).await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert_eq!(launches.get(), 3);
        assert!(
            seen_methods(&others).ends_with(&["session/resume", "session/prompt"]),
            "{others:?}"
        );
        assert_no_other_response(&others);
    });
}

/// Test that a close of a session whose agent exited gets its reply at
/// once and starts no agent. The session has nothing left to close.
///   1. Start a session and make its agent exit
///   2. Send `session/close` for the session
///   3. Check the OK response and that no new agent started
#[test]
fn close_after_agent_exit_starts_no_agent() {
    block_on_local(async {
        let (mut client, launches) = session_after_agent_exit(&[]).await;
        let params = json!({ "sessionId": client.sid });
        let (response, _) = client.request(3, "session/close", params).await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert_eq!(launches.get(), 1);
    });
}

/// Test that each request that comes while a reopen fails gets exactly one
/// response, and that a later request opens the session. A request that
/// gets no response makes the editor wait forever.
///   1. Start a session with an agent that refuses the first reopen
///   2. Make the agent exit
///   3. Send two prompts at once. The first one starts the reopen
///   4. Check that the first prompt gets an error and the second prompt
///      gets one response
///   5. Send a third prompt and check that it gets its answer
#[test]
fn requests_during_failed_reopen_get_one_response_each() {
    block_on_local(async {
        let dir = temp_dir();
        let flag = dir.path().join("fail");
        std::fs::write(&flag, "").unwrap();
        let flag = flag.to_str().unwrap();
        let (mut client, _) = session_after_agent_exit(&[("AGENT_REOPEN_FAIL", flag)]).await;
        let sid = client.sid.clone();
        client.send_prompt(3, &sid).await;
        client.send_prompt(4, &sid).await;

        let mut responses = Vec::new();
        while responses.len() < 2 {
            let msg = client.recv().await;
            if msg.get("method").is_none() {
                responses.push(msg);
            }
        }
        responses.sort_by_key(|m| m["id"].as_i64());
        assert_eq!(responses[0]["id"], 3, "{responses:?}");
        assert_eq!(responses[0]["error"]["code"], -32603, "{responses:?}");
        assert_eq!(responses[1]["id"], 4, "{responses:?}");

        let (response, others) = client.prompt(5, &sid).await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert_no_other_response(&others);
    });
}

/// Test that a request to a session whose agent exited gets an error when
/// the new agent can neither resume nor load. The agent cannot open the
/// session, and the client must not wait.
///   1. Start a session with an agent without resume and load
///   2. Make the agent exit
///   3. Send a prompt and check the error
#[test]
fn prompt_after_agent_exit_fails_when_agent_cannot_reopen() {
    block_on_local(async {
        let mut init = full_agent_init();
        let caps = &mut init["agentCapabilities"];
        caps["loadSession"] = json!(false);
        caps["sessionCapabilities"]
            .as_object_mut()
            .unwrap()
            .remove("resume");
        let init = init.to_string();
        let (mut client, _) = session_after_agent_exit(&[("AGENT_INIT", &init)]).await;
        let sid = client.sid.clone();
        let (response, _) = client.prompt(3, &sid).await;
        let message = response["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(REOPEN_NOT_SUPPORTED), "{response}");
    });
}

/// Test that a request over the limit of the messages that wait for a
/// reopen gets an error at once. The memory use must have a limit.
///   1. Start a session with an agent that waits 1 s before its handshake
///      reply
///   2. Make the agent exit
///   3. Send 65 prompts at once. The first one starts the reopen
///   4. Check that only the last prompt gets the queue full error
#[test]
fn request_over_reopen_queue_limit_is_refused() {
    block_on_local(async {
        let (mut client, _) = session_after_agent_exit(&[("AGENT_INIT_DELAY", "1")]).await;
        let sid = client.sid.clone();
        for id in 3..=67 {
            client.send_prompt(id, &sid).await;
        }
        let mut refused = Vec::new();
        let mut responses = 0;
        while responses < 65 {
            let msg = client.recv().await;
            if msg.get("method").is_some() {
                continue;
            }
            responses += 1;
            if msg["error"]["message"] == REOPEN_QUEUE_FULL {
                refused.push(msg["id"].clone());
            }
        }
        assert_eq!(refused, [json!(67)]);
    });
}

/// Test that a refused session load starts no session: the client gets no
/// sandbox notices for it, and a later agent exit gives no reopen. The
/// session does not exist in the agent.
///   1. Use a sandbox with a notice, and an agent that refuses all
///      requests and exits after 1 s
///   2. Send `session/load` and check the error
///   3. Wait for the agent exit
///   4. Send a prompt to the session and check the unknown session error
///   5. Check that no notice came and that no new agent launched
#[test]
fn refused_load_claims_no_session() {
    block_on_local(async {
        let notices = NoticeLog::new();
        notices.push("config changed".into());
        let error = r#"{"code":-32002,"message":"Resource not found"}"#;
        let env = [("AGENT_ERROR", error), ("AGENT_DIE", "1")];
        let launcher = LocalProcessLauncher::new(&env).with_notices(&notices);
        let launches = launcher.launches.clone();
        let exited = launcher.exited.clone();
        let mut client = TestClient::start(launcher, test_config(true));
        client
            .initialize(json!({ "session": { "notices": {} } }))
            .await;
        let params = json!({ "sessionId": "a:x", "cwd": "/projects/a", "mcpServers": [] });
        let (response, mut others) = client.request(2, "session/load", params).await;
        assert_eq!(response["error"]["code"], -32002, "{response}");

        tokio::time::timeout(Duration::from_secs(10), async {
            while exited.get() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("agent exit in time");
        let (response, more) = client.prompt(3, "a:x").await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        others.extend(more);
        assert!(
            others.iter().all(|m| m.pointer("/params/update").is_none()),
            "{others:?}"
        );
        assert_eq!(launches.get(), 1);
    });
}

/// Test that a reopen that the agent does not answer fails after the
/// handshake time limit, and that the next request tries again. Else the
/// waiting requests of the session wait forever.
///   1. Start a session with an agent that never answers a resume, with a
///      time limit of 1 s
///   2. Make the agent exit
///   3. Send a prompt and check that it gets the time limit error
///   4. Send a second prompt and check that a third agent starts for it
#[test]
fn reopen_without_answer_fails_after_time_limit() {
    block_on_local(async {
        let config = Config {
            handshake_timeout: Duration::from_secs(1),
            ..test_config(true)
        };
        let env = [("AGENT_HOLD", "session/resume")];
        let (mut client, launches) = session_after_agent_exit_with(&env, config).await;
        let sid = client.sid.clone();
        let (response, others) = client.prompt(3, &sid).await;
        let message = response["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(&reopen_timeout(1)), "{response}");
        assert_no_other_response(&others);

        let (response, _) = client.prompt(4, &sid).await;
        assert_eq!(response["error"]["code"], -32603, "{response}");
        assert_eq!(launches.get(), 3);
    });
}

/// Get the first `session/request_permission` in `msgs`. Panics if there
/// is none.
fn permission_request(msgs: &[Value]) -> Value {
    msgs.iter()
        .find(|m| m["method"] == "session/request_permission")
        .unwrap_or_else(|| panic!("{msgs:?}"))
        .clone()
}

/// Test that the open request of an agent that exited ends, and that a
/// late client answer to it does not reach the next agent. Else an "allow"
/// in an old permission dialog approves a new tool call.
///   1. Start a session with an agent that asks for a permission with the
///      same id at each prompt, and send a prompt
///   2. Make the agent exit, and check that the client gets a cancel of the
///      open permission request
///   3. Send a prompt. A new agent opens the session and asks again
///   4. Check that the new request has a new id
///   5. Answer the old request and then the new request, and check that the
///      new agent gets only the answer of the new request
#[test]
fn agent_request_of_exited_agent_ends_and_its_answer_is_dropped() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[("AGENT_ASK", "5")]);
        let mut client = TestClient::session(launcher, test_config(true), "/projects/a").await;
        let sid = client.sid.clone();
        let (_, others) = client.prompt(3, &sid).await;
        let old = permission_request(&others);

        client
            .notify("test/exit", json!({ "sessionId": sid }))
            .await;
        let cancel = loop {
            let msg = client.recv().await;
            if msg["method"] == CANCEL_REQUEST {
                break msg;
            }
        };
        assert_eq!(cancel["params"]["requestId"], old["id"], "{cancel}");

        let (_, others) = client.prompt(4, &sid).await;
        let new = permission_request(&others);
        assert_ne!(new["id"], old["id"]);

        for (ask, option) in [(&old, "old"), (&new, "new")] {
            let result = json!({ "outcome": { "outcome": "selected", "optionId": option } });
            let answer = json!({ "jsonrpc": "2.0", "id": ask["id"], "result": result });
            client.send(&answer).await;
        }
        let answer = loop {
            let msg = client.recv().await;
            if let Some(seen) = msg.pointer("/params/seen")
                && seen.get("result").is_some()
            {
                break seen.clone();
            }
        };
        assert_eq!(answer["id"], 5, "{answer}");
        assert_eq!(answer["result"]["outcome"]["optionId"], "new", "{answer}");
    });
}

/// Test that the reopen error text of the agent cannot fake a line in the
/// log. The agent is not trusted, and the user reads the log.
///   1. Start a session with an agent that refuses the first reopen with
///      an error text with a newline
///   2. Make the agent exit and send a prompt
///   3. Check that the log has the reopen failure, and the newline only in
///      escaped form
#[test]
fn reopen_error_of_agent_cannot_fake_log_line() {
    block_on_local(async {
        let logs = capture_logs();
        let dir = temp_dir();
        let flag = dir.path().join("fail");
        std::fs::write(&flag, "").unwrap();
        let flag = flag.to_str().unwrap();
        let error = r#"{"code":-32002,"message":"bad\nFAKE LOG LINE"}"#;
        let env = [("AGENT_REOPEN_FAIL", flag), ("AGENT_REOPEN_ERROR", error)];
        let (mut client, _) = session_after_agent_exit(&env).await;
        let sid = client.sid.clone();
        let (response, _) = client.prompt(3, &sid).await;
        assert!(response.get("error").is_some(), "{response}");
        let text = logs.text();
        assert!(text.contains("session reopen failed"), "{text}");
        assert!(!text.contains("\nFAKE LOG LINE"), "{text}");
    });
}

/// Test that a reopen whose sandbox does not start gives the waiting
/// request the error data of the sandbox process. The editor shows only
/// the reply, so the reason must be in it.
///   1. Start a session, with an agent launcher whose later launches fail
///      as a sandbox process that stops in its start
///   2. Make the agent exit and send a prompt
///   3. Check that the error has the reopen failure and the exit code and
///      output of the sandbox process
#[test]
fn failed_reopen_start_keeps_error_data() {
    block_on_local(async {
        let launcher = LocalProcessLauncher::new(&[]).with_good_launches(1);
        let (mut client, _) = session_after_agent_exit_in(launcher, test_config(true)).await;
        let sid = client.sid.clone();
        let (response, _) = client.prompt(3, &sid).await;
        let error = &response["error"];
        let message = error["message"].as_str().unwrap_or_default();
        assert!(message.contains("the vault is locked"), "{response}");
        assert_eq!(error["data"]["exitCode"], 4, "{response}");
        assert_eq!(error["data"]["output"], "sandbox output", "{response}");
    });
}

/// Test that the client answer and cancel of an agent request reach the
/// agent while the reopen waits. The agent can wait for the answer before
/// it answers the reopen.
///   1. Start a session with an agent that never answers a resume, and
///      sends a permission request when it gets the resume
///   2. Make the agent exit and send a prompt. A new agent gets the resume
///   3. Cancel and then answer the permission request
///   4. Check that the agent gets the cancel and the answer with its own id
#[test]
fn answer_of_agent_request_does_not_wait_for_reopen() {
    block_on_local(async {
        let ask = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{}}"#;
        let env = [("AGENT_HOLD", "session/resume"), ("AGENT_ON_HOLD", ask)];
        let (mut client, _) = session_after_agent_exit(&env).await;
        let sid = client.sid.clone();
        client.send_prompt(3, &sid).await;
        let ask = loop {
            let msg = client.recv().await;
            if msg["method"] == "session/request_permission" {
                break msg;
            }
        };
        client
            .notify(CANCEL_REQUEST, json!({ "requestId": ask["id"] }))
            .await;
        let answer = json!({ "jsonrpc": "2.0", "id": ask["id"], "result": {} });
        client.send(&answer).await;
        let mut got = Vec::new();
        while got.len() < 2 {
            let msg = client.recv().await;
            let Some(seen) = msg.pointer("/params/seen") else {
                continue;
            };
            if seen["method"] == CANCEL_REQUEST {
                got.push(seen["params"]["requestId"].clone());
            } else if seen.get("result").is_some() {
                got.push(seen["id"].clone());
            }
        }
        assert_eq!(got, [json!(7), json!(7)]);
    });
}
