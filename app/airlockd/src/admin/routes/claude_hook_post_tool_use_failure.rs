//! Hook after a failed tool call.
//!
//! Claude Code calls this hook after a tool call fails. The hook checks if the
//! host denied network access while the tool ran. If yes, it tells Claude the
//! probable cause of the failure.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::state::AdminState;

/// Context message for Claude when a deny occurred during the tool call.
const CONTEXT_MESSAGE: &str = "\
A network request was denied by a network policy during this tool \
call. The tool's failure is likely caused by the denied request. \
Ask the user for more instructions before retrying.";

/// Hook request body. Only the used fields.
#[derive(Deserialize)]
pub struct Payload {
    tool_use_id: Option<String>,
}

/// Remove the start record of the tool call and compare it with the latest
/// deny time.
/// Returns:
///   If the host reported a deny at or after the tool start, a
///   `PostToolUseFailure` `hookSpecificOutput` with [`CONTEXT_MESSAGE`] as
///   `additionalContext`, so Claude can tell the user the real cause.
///   Otherwise an empty JSON object, so the failure continues normally.
pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    let Some(id) = p.tool_use_id else {
        debug!("post-tool-use-failure: payload missing tool_use_id, passing through");
        return Json(json!({}));
    };
    let Some(started_at) = state.tool_tracker.take(&id) else {
        debug!("post-tool-use-failure: {id} has no start record, passing through");
        return Json(json!({}));
    };
    let Some(last_deny) = state.deny_tracker.last() else {
        debug!("post-tool-use-failure: {id} — no denies reported, passing through");
        return Json(json!({}));
    };
    if last_deny < started_at {
        debug!(
            "post-tool-use-failure: {id} — last deny {last_deny}ms predates tool start \
             {started_at}ms, passing through"
        );
        return Json(json!({}));
    }
    debug!(
        "post-tool-use-failure: {id} — deny at {last_deny}ms overlaps tool start {started_at}ms, \
         injecting context"
    );
    Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUseFailure",
            "additionalContext": CONTEXT_MESSAGE,
        }
    }))
}
