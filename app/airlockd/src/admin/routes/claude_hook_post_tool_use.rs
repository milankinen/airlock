//! Hook after a tool call.
//!
//! Claude Code calls this hook after a tool call. It can also call it after a
//! failed call. The hook records the end of the call. A later temporary
//! network request can then find the denies of the call, also when the
//! command did not fail on the deny. It sends nothing back to Claude.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::now_ms;
use crate::admin::state::AdminState;

/// Hook request body. Only the used fields.
#[derive(Deserialize)]
pub struct Payload {
    tool_use_id: Option<String>,
}

/// Record the end of the tool call. Always returns an empty JSON object.
pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    if let Some(id) = p.tool_use_id {
        let found = state.tool_tracker.finish(&id, now_ms()).is_some();
        debug!("post-tool-use: {id} (record found: {found})");
    } else {
        debug!("post-tool-use: payload missing tool_use_id, skipped");
    }
    Json(json!({}))
}
