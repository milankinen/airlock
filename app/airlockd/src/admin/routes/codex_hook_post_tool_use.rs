//! Codex hook after a tool call.
//!
//! Codex calls this hook after each tool call. Codex has no hook for failed
//! calls, and shell results have no exit code. Thus the hook checks each
//! call: if the network policy blocked hosts while the tool ran, it tells
//! Codex the blocked hosts as the probable cause of a failure.

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
/// Duplicate hooks use the same time window.
///
/// Returns:
///   If the host blocked hosts while the tool ran, a `PostToolUse`
///   `hookSpecificOutput` with the blocked hosts in `additionalContext`.
///   Else an empty JSON object.
pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    let Some(id) = p.tool_use_id else {
        debug!("codex post-tool-use: payload missing tool_use_id, passing through");
        return Json(json!({}));
    };
    // Codex refuses unknown fields and an event name that is not the name
    // of this event.
    match tool_end_context(&state, &id, "codex post-tool-use") {
        Some(context) => Json(json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": context,
            }
        })),
        None => Json(json!({})),
    }
}
