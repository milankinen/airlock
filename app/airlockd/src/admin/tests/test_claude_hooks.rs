//! Claude Code HTTP hooks of the admin service: the hooks tell Claude when
//! a network deny is the probable cause of a failed tool call.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::admin::AdminState;
use crate::test_cfg::{block_on_local, post_hook, serve_admin};

const PRE: &str = "/claude/hooks/pre-tool-use";
const POST: &str = "/claude/hooks/post-tool-use";
const FAILURE: &str = "/claude/hooks/post-tool-use-failure";

/// Return the current wall-clock time in milliseconds since the epoch.
fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Make a hook payload for tool call `id`. The hooks read only
/// `tool_use_id`, so all three hooks can use the same payload.
fn tool(id: &str) -> Value {
    json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": id })
}

/// Test that a tool failure after a network deny gets the deny context, and
/// gets it only one time. Claude must learn the probable cause of the failure.
///   1. Send the pre-tool-use hook for a tool call
///   2. Record a network deny after the tool start
///   3. Send the failure hook and check that the reply has the deny context
///   4. Send the failure hook again and check that the reply is empty
#[test]
fn tool_failure_after_policy_deny_gets_deny_context_once() {
    block_on_local(async {
        let state = AdminState::new();
        let addr = serve_admin(state.clone()).await;

        assert_eq!(post_hook(addr, PRE, &tool("toolu_1")).await, json!({}));
        state.deny_tracker.record(now_ms());
        let reply = post_hook(addr, FAILURE, &tool("toolu_1")).await;

        let output = &reply["hookSpecificOutput"];
        assert_eq!(output["hookEventName"], "PostToolUseFailure");
        assert!(
            output["additionalContext"]
                .as_str()
                .unwrap()
                .contains("denied by a network policy"),
            "{reply}"
        );
        assert_eq!(post_hook(addr, FAILURE, &tool("toolu_1")).await, json!({}));
    });
}

/// Test that a tool failure without a deny during the tool call gets an
/// empty reply. Claude must not get a false deny hint.
///   1. Fail a tool call when no deny occurred
///   2. Fail a tool call that started after an old deny
///   3. Fail a tool call that has no start record, after a new deny
///   4. Send payloads without a tool call ID to the hooks
///   5. Check that each reply is empty
#[test]
fn tool_failure_without_overlapping_deny_passes_through() {
    block_on_local(async {
        let state = AdminState::new();
        let addr = serve_admin(state.clone()).await;

        post_hook(addr, PRE, &tool("no_deny")).await;
        assert_eq!(post_hook(addr, FAILURE, &tool("no_deny")).await, json!({}));

        // A deny one minute ago is before the start of the next tool call.
        state.deny_tracker.record(now_ms() - 60_000);
        post_hook(addr, PRE, &tool("old_deny")).await;
        assert_eq!(post_hook(addr, FAILURE, &tool("old_deny")).await, json!({}));

        // A new deny does not help a tool call that has no start record.
        state.deny_tracker.record(now_ms());
        assert_eq!(post_hook(addr, FAILURE, &tool("unknown")).await, json!({}));
        assert_eq!(post_hook(addr, FAILURE, &json!({})).await, json!({}));
        assert_eq!(post_hook(addr, PRE, &json!({})).await, json!({}));
    });
}

/// Test that the post-tool-use hook removes the start record of a successful
/// tool call. A later failure hook for the same ID must not get deny context.
///   1. Send the pre-tool-use and post-tool-use hooks for a tool call
///   2. Record a network deny
///   3. Send the failure hook for the same ID and check that the reply is
///      empty
#[test]
fn successful_tool_releases_its_start_record() {
    block_on_local(async {
        let state = AdminState::new();
        let addr = serve_admin(state.clone()).await;

        post_hook(addr, PRE, &tool("toolu_ok")).await;
        assert_eq!(post_hook(addr, POST, &tool("toolu_ok")).await, json!({}));
        assert_eq!(post_hook(addr, POST, &json!({})).await, json!({}));
        state.deny_tracker.record(now_ms());

        assert_eq!(post_hook(addr, FAILURE, &tool("toolu_ok")).await, json!({}));
    });
}
