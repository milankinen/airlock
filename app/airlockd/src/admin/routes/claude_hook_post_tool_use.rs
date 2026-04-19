//! `POST /claude/hooks/post-tool-use` — drop the pre-hook's start record
//! on successful tool completion.
//!
//! Nothing to inject back to Claude; the route exists only to release
//! the `ToolTracker` entry so it doesn't linger until eviction.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::admin::state::AdminState;

#[derive(Deserialize)]
pub struct Payload {
    tool_use_id: Option<String>,
}

pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    if let Some(id) = p.tool_use_id {
        let found = state.tool_tracker.take(&id).is_some();
        debug!("post-tool-use: {id} (start-record found: {found})");
    } else {
        debug!("post-tool-use: payload missing tool_use_id, skipped");
    }
    Json(json!({}))
}
