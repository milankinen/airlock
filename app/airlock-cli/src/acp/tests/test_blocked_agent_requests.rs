//! Tests for the agent messages that the proxy does not give to the
//! client.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalStarter, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that in the default mode the client gets only the allowed agent
/// messages, and that the agent gets an error for each refused request.
/// Else the agent can use host features through the editor.
///   1. Use an agent that sends requests for host features, an extension
///      request, a URL prompt, an MCP message, a batch, a line that is not
///      JSON, a message with an id and a method that is not a string, an
///      extension notification, a permission request and a form prompt
///   2. Start a session and send one more request, so that all agent
///      output is handled
///   3. Check that the client got only the permission request and the
///      form prompt
///   4. Check the error codes that the agent got, and that the batch and
///      the message with the bad method got no reply
#[test]
fn agent_messages_out_of_allowlist_never_reach_client() {
    let say = [
        r#"{"jsonrpc":"2.0","id":"t1","method":"terminal/create","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"t2","method":"fs/write_text_file","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"t3","method":"_zed/ext","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"t4","method":"elicitation/create","params":{"mode":"url","url":"https://x.test"}}"#,
        r#"{"jsonrpc":"2.0","id":"t5","method":"mcp/message","params":{}}"#,
        r#"[{"jsonrpc":"2.0","id":"t6","method":"fs/read_text_file","params":{}}]"#,
        "not json",
        r#"{"jsonrpc":"2.0","id":"t7","method":5,"result":{}}"#,
        r#"{"jsonrpc":"2.0","method":"_zed/notify","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"ok1","method":"session/request_permission","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"ok2","method":"elicitation/create","params":{"mode":"form","requestedSchema":{}}}"#,
    ]
    .join("\n");
    block_on_local(async {
        let starter = LocalStarter::new(&[("AGENT_SAY", &say)]);
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (_, mut others) = client.request(2, "session/new", params).await;
        let prompt = json!({ "sessionId": "s", "prompt": [] });
        let (_, more) = client.request(3, "session/prompt", prompt).await;
        others.extend(more);

        let methods: Vec<&Value> = others
            .iter()
            .filter(|m| m["method"] != "session/update")
            .map(|m| &m["id"])
            .collect();
        assert_eq!(methods, [&json!("ok1"), &json!("ok2")]);

        let errors: BTreeMap<String, Value> = seen(&others)
            .into_iter()
            .filter(|m| m.get("error").is_some())
            .map(|m| {
                (
                    m["id"].as_str().unwrap().to_string(),
                    m["error"]["code"].clone(),
                )
            })
            .collect();
        let expected: BTreeMap<String, Value> = [
            ("t1", -32601),
            ("t2", -32601),
            ("t3", -32601),
            ("t4", -32602),
            ("t5", -32601),
        ]
        .into_iter()
        .map(|(id, code)| (id.to_string(), json!(code)))
        .collect();
        assert_eq!(errors, expected);
    });
}

/// Test that the proxy applies the same rules to the agent messages that
/// come before the agent handshake completes. The agent can write before
/// it replies to the handshake.
///   1. Use an agent that sends a request for a host feature, an extension
///      notification and a permission request before its handshake reply
///   2. Start a session
///   3. Check that the client got only the permission request
///   4. Check that the agent got an error for the refused request
#[test]
fn agent_messages_before_handshake_are_filtered() {
    let pre = [
        r#"{"jsonrpc":"2.0","id":"t1","method":"terminal/create","params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"_zed/notify","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"ok1","method":"session/request_permission","params":{}}"#,
    ]
    .join("\n");
    block_on_local(async {
        let starter = LocalStarter::new(&[("AGENT_PRE", &pre)]);
        let mut client = TestClient::start(starter, test_config(true));
        client.initialize(json!({})).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
        let (response, others) = client.request(2, "session/new", params).await;
        assert!(response.get("result").is_some(), "{response}");

        let ids: Vec<&Value> = others
            .iter()
            .filter(|m| m["method"] != "session/update")
            .map(|m| &m["id"])
            .collect();
        assert_eq!(ids, [&json!("ok1")]);
        let errors: Vec<(Value, Value)> = seen(&others)
            .into_iter()
            .filter(|m| m.get("error").is_some())
            .map(|m| (m["id"].clone(), m["error"]["code"].clone()))
            .collect();
        assert_eq!(errors, [(json!("t1"), json!(-32601))]);
    });
}
