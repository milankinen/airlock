//! `POST /claude/hooks/post-tool-use-failure` — correlate a failed tool
//! call with host-side network denies.
//!
//! If a deny was reported at or after the tool's recorded start, return
//! a `PostToolUseFailure` `hookSpecificOutput` with `additionalContext`
//! so Claude can surface the real cause to the user. Otherwise respond
//! with an empty object so the failure propagates normally. Either way
//! the `ToolTracker` entry is removed.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::state::AdminState;

const CONTEXT_MESSAGE: &str = "\
A network request was denied by a network policy during this tool \
call. The tool's failure is likely caused by the denied request. \
Ask the user for more instructions before retrying.";

#[derive(Deserialize)]
pub struct Payload {
    tool_use_id: Option<String>,
}

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
