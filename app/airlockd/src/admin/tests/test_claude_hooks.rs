use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::admin::AdminState;
use crate::test_cfg::{block_on_local, post_hook, serve_admin};

const PRE: &str = "/claude/hooks/pre-tool-use";
const POST: &str = "/claude/hooks/post-tool-use";
const FAILURE: &str = "/claude/hooks/post-tool-use-failure";

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn tool(id: &str) -> Value {
    json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": id })
}

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

#[test]
fn tool_failure_without_overlapping_deny_passes_through() {
    block_on_local(async {
        let state = AdminState::new();
        let addr = serve_admin(state.clone()).await;

        post_hook(addr, PRE, &tool("no_deny")).await;
        assert_eq!(post_hook(addr, FAILURE, &tool("no_deny")).await, json!({}));

        state.deny_tracker.record(now_ms() - 60_000);
        post_hook(addr, PRE, &tool("old_deny")).await;
        assert_eq!(post_hook(addr, FAILURE, &tool("old_deny")).await, json!({}));

        state.deny_tracker.record(now_ms());
        assert_eq!(post_hook(addr, FAILURE, &tool("unknown")).await, json!({}));
        assert_eq!(post_hook(addr, FAILURE, &json!({})).await, json!({}));
        assert_eq!(post_hook(addr, PRE, &json!({})).await, json!({}));
    });
}

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
