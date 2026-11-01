//! Hook after a failed tool call.
//!
//! Claude Code calls this hook after a tool call fails. The hook checks if the
//! network policy blocked hosts while the tool ran. If yes, it tells Claude
//! the blocked hosts as the probable cause of the failure.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::blocked_hosts::tool_end_context;
use crate::admin::state::AdminState;

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
    match tool_end_context(&state, &id, "post-tool-use-failure") {
        Some(context) => Json(json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUseFailure",
                "additionalContext": context,
            }
        })),
        None => Json(json!({})),
    }
}
