//! GitHub Copilot CLI hooks of the admin service: the hooks tell Copilot
//! the hosts that the network policy blocked during a failed tool call.

use airlock_test_utils::serve;
use serde_json::{Value, json};

use crate::admin::server::router;
use crate::admin::{AdminState, Capabilities, DenyEntry, RuleEditing, now_ms};
use crate::test_cfg::{block_on_local, post_hook, serve_admin};

const PRE: &str = "/copilot/hooks/pre-tool-use";
const POST: &str = "/copilot/hooks/post-tool-use";
const FAILURE: &str = "/copilot/hooks/post-tool-use-failure";
const SESSION_START: &str = "/copilot/hooks/session-start";

/// Make a Copilot hook payload for a `bash` tool call with `args`.
fn bash(args: &Value) -> Value {
    json!({
        "sessionId": "5f1c-session",
        "timestamp": now_ms(),
        "cwd": "/work",
        "toolName": "bash",
        "toolArgs": args,
    })
}

/// Test that a failed tool call after a deny tells Copilot the blocked
/// hosts. Copilot gives no tool call ID, so the hooks find the call from
/// its session, tool name and arguments.
///   1. Send the pre-tool-use hook with the arguments as a JSON string
///   2. Record a deny
///   3. Send the failure hook for other arguments and check that the reply
///      is empty
///   4. Send the success hook for the same arguments and check that the
///      reply is empty
///   5. Send the failure hook with the same arguments as an object with
///      other key order and check the exact output
#[test]
fn failure_with_same_tool_args_reports_blocked_hosts() {
    block_on_local(async {
        let state = AdminState::new();
        let addr = serve_admin(state.clone()).await;
        let args = r#"{"command":"curl https://example.org","description":"Fetch"}"#;

        assert_eq!(post_hook(addr, PRE, &bash(&json!(args))).await, json!({}));
        state.deny_tracker.record(DenyEntry {
            epoch: now_ms(),
            host: "example.org".into(),
            port: 443,
            grantable: true,
        });
        let other = bash(&json!({ "command": "ls", "description": "List" }));
        assert_eq!(post_hook(addr, FAILURE, &other).await, json!({}));

        let mut failure = bash(&json!({
            "description": "Fetch",
            "command": "curl https://example.org",
        }));
        failure["error"] = json!("curl: (6) Could not resolve host: example.org");
        assert_eq!(post_hook(addr, POST, &failure).await, json!({}));
        let reply = post_hook(addr, FAILURE, &failure).await;

        assert_eq!(
            reply,
            json!({
                "additionalContext": "The tool made network calls that the sandbox network \
                    policy blocked. Blocked hosts:\n- example.org:443",
            })
        );
        assert_eq!(post_hook(addr, FAILURE, &json!({})).await, json!({}));
    });
}

/// Test that the session start hook gives Copilot the instructions with the
/// MCP tool names of Copilot, in the flat output format of Copilot.
///   1. Serve the admin routes with grants and rule editing
///   2. Send the session start hook
///   3. Check the output fields and the tool names in the context text
#[test]
fn session_start_gives_instructions_with_copilot_tool_names() {
    block_on_local(async {
        let caps = Capabilities {
            grants: true,
            rule_editing: RuleEditing::All,
        };
        let addr = serve(router(AdminState::new(), None, caps)).await;
        let reply = post_hook(addr, SESSION_START, &json!({ "source": "new" })).await;
        let output = reply.as_object().unwrap();
        assert_eq!(output.len(), 1, "{reply}");
        let text = output["additionalContext"].as_str().unwrap();
        assert!(text.contains("`airlock-execute`"), "{text}");
        assert!(text.contains("`airlock-upsert_rule`"), "{text}");
        assert!(!text.contains("mcp__"), "{text}");
    });
}
