//! Hook after a successful tool call.
//!
//! Claude Code calls this hook after a tool call succeeds. The hook removes the
//! start record of the call, so the record does not stay in memory. It sends
//! nothing back to Claude.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::state::AdminState;

/// Hook request body. Only the used fields.
#[derive(Deserialize)]
pub struct Payload {
    tool_use_id: Option<String>,
}

/// Remove the start record of the tool call. Always returns an empty JSON
/// object.
pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    if let Some(id) = p.tool_use_id {
        let found = state.tool_tracker.take(&id).is_some();
        debug!("post-tool-use: {id} (start-record found: {found})");
    } else {
        debug!("post-tool-use: payload missing tool_use_id, skipped");
    }
    Json(json!({}))
}
