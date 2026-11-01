//! Codex hooks of the admin service: the hooks tell Codex the hosts that
//! the network policy blocked during a tool call.

use airlock_test_utils::serve;
use serde_json::{Value, json};

use crate::admin::server::router;
use crate::admin::{AdminState, Capabilities, DenyEntry, RuleEditing, now_ms};
use crate::test_cfg::{block_on_local, post_hook, serve_admin};

const PRE: &str = "/codex/hooks/pre-tool-use";
const POST: &str = "/codex/hooks/post-tool-use";
const SESSION_START: &str = "/codex/hooks/session-start";

/// Make a Codex hook payload for the shell tool call `id`.
fn shell(event: &str, id: &str) -> Value {
    json!({
        "session_id": "019a-session",
        "hook_event_name": event,
        "tool_name": "shell",
        "tool_use_id": id,
        "tool_input": { "command": ["curl", "https://example.org"] },
        "tool_response": "curl: (6) Could not resolve host: example.org",
    })
}

/// Test that the post-tool-use hook tells Codex the blocked hosts of the
/// call, with the event name of the hook. Codex has no failure hook and
/// refuses output with a different event name.
///   1. Run a call with no deny and check that the reply is empty
///   2. Send the pre-tool-use hook for a second call, then record a deny
///   3. Send the post-tool-use hook and check the exact output
///   4. Send the post-tool-use hook again and check that the reply is the
///      same
#[test]
fn post_tool_use_after_denies_reports_blocked_hosts() {
    block_on_local(async {
        let state = AdminState::new();
        let addr = serve_admin(state.clone()).await;

        post_hook(addr, PRE, &shell("PreToolUse", "call_0")).await;
        let reply = post_hook(addr, POST, &shell("PostToolUse", "call_0")).await;
        assert_eq!(reply, json!({}));

        let pre = post_hook(addr, PRE, &shell("PreToolUse", "call_1")).await;
        assert_eq!(pre, json!({}));
        state.deny_tracker.record(DenyEntry {
            epoch: now_ms(),
            host: "example.org".into(),
            port: 443,
            grantable: true,
        });
        let reply = post_hook(addr, POST, &shell("PostToolUse", "call_1")).await;

        assert_eq!(
            reply,
            json!({ "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": "The tool made network calls that the sandbox network \
                    policy blocked. Blocked hosts:\n- example.org:443",
            }})
        );
        assert_eq!(
            post_hook(addr, POST, &shell("PostToolUse", "call_1")).await,
            reply
        );
    });
}

/// Test that the session start hook gives Codex the instructions with the
/// MCP tool names of Codex, in the output format of Codex.
///   1. Serve the admin routes with grants and rule editing
///   2. Send the session start hook
///   3. Check the event name and the tool names in the context text
#[test]
fn session_start_gives_instructions_with_codex_tool_names() {
    block_on_local(async {
        let caps = Capabilities {
            grants: true,
            rule_editing: RuleEditing::All,
        };
        let addr = serve(router(AdminState::new(), None, caps)).await;
        let reply = post_hook(addr, SESSION_START, &json!({ "source": "startup" })).await;
        let output = reply["hookSpecificOutput"].as_object().unwrap();
        assert_eq!(output.len(), 2, "{reply}");
        assert_eq!(output["hookEventName"], "SessionStart");
        let text = output["additionalContext"].as_str().unwrap();
        assert!(text.contains("`mcp__airlock__execute`"), "{text}");
        assert!(text.contains("`mcp__airlock__upsert_rule`"), "{text}");
    });
}
