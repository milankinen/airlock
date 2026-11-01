//! Hook after a failed tool call.
//!
//! Claude Code calls this hook after a tool call fails. The hook checks if the
//! network policy blocked hosts while the tool ran. If yes, it tells Claude
//! the blocked hosts as the probable cause of the failure. It tells apart
//! the hosts that a temporary grant can allow and the hosts that it cannot
//! allow (deny rules, local addresses).

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::deny_tracker::blocked_targets;
use crate::admin::now_ms;
use crate::admin::state::AdminState;

/// Maximum number of blocked hosts in the context.
const MAX_HOSTS: usize = 20;

/// Hook request body. Only the used fields.
#[derive(Deserialize)]
pub struct Payload {
    tool_use_id: Option<String>,
}

/// Record the end of the tool call and look for denies in its time window.
/// The first hook stores the end time. Duplicate hooks use the same time
/// window.
///
/// Returns:
///   If the host blocked hosts while the tool ran, a `PostToolUseFailure`
///   `hookSpecificOutput` with the blocked hosts in `additionalContext`.
///   Else an empty JSON object, so the failure continues normally.
pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    let Some(id) = p.tool_use_id else {
        debug!("post-tool-use-failure: payload missing tool_use_id, passing through");
        return Json(json!({}));
    };
    let now = now_ms();
    let Some(window) = state.tool_tracker.finish(&id, now) else {
        debug!("post-tool-use-failure: {id} has no start record, passing through");
        return Json(json!({}));
    };
    // The first hook ends the window now. Denies that the host reports
    // later than this hook are not visible here.
    let denies = state
        .deny_tracker
        .between(window.start, window.deny_end(now));
    // Only hosts with safe names go to the agent. Socket paths have none.
    let (grantable, fixed): (Vec<_>, Vec<_>) =
        denies.into_iter().partition(|entry| entry.grantable);
    let fixed = blocked_targets(&fixed);
    // A host that a deny rule blocked once stays in the "cannot allow" list.
    let targets: Vec<String> = blocked_targets(&grantable)
        .into_iter()
        .filter(|t| !fixed.contains(t))
        .collect();
    if targets.is_empty() && fixed.is_empty() {
        debug!("post-tool-use-failure: {id} — no blocked host in the tool window, passing through");
        return Json(json!({}));
    }
    let context = blocked_context(&targets, &fixed);
    debug!(
        "post-tool-use-failure: {id} — {} grantable and {} fixed blocked host(s), injecting context",
        targets.len(),
        fixed.len()
    );
    Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUseFailure",
            "additionalContext": context,
        }
    }))
}

/// Make the context that tells Claude the blocked hosts.
/// Args:
///  - `targets`: Blocked `host:port` targets that a temporary grant can
///    allow, with no duplicates
///  - `fixed`: Blocked targets that no grant can allow, with no duplicates
fn blocked_context(targets: &[String], fixed: &[String]) -> String {
    let mut text = String::new();
    if !targets.is_empty() {
        text.push_str(
            "The tool made network calls that the sandbox network policy blocked. Blocked hosts:",
        );
        push_hosts(&mut text, targets);
    }
    if !fixed.is_empty() {
        if text.is_empty() {
            text.push_str("The tool made network calls that the sandbox does not allow at all ");
        } else {
            text.push_str("\nThe sandbox does not allow these hosts at all ");
        }
        text.push_str("(deny rules or local addresses):");
        push_hosts(&mut text, fixed);
        text.push_str("\nDo not request access to these hosts. Ask the user for more details.");
    }
    text
}

/// Add one line for each target, at most `MAX_HOSTS` lines and a count of
/// the others.
fn push_hosts(text: &mut String, targets: &[String]) {
    for t in targets.iter().take(MAX_HOSTS) {
        let _ = write!(text, "\n- {t}");
    }
    if targets.len() > MAX_HOSTS {
        let _ = write!(text, "\n- and {} more", targets.len() - MAX_HOSTS);
    }
}
