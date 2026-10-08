//! Hook before a tool call.
//!
//! Claude Code calls this hook before it runs a tool. The hook records the
//! start time of the tool call. The failure hook uses this time to decide if a
//! network deny caused a failure.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Record the current time as the start of the tool call. Always returns an
/// empty JSON object, so the hook does not change the tool behavior.
pub async fn handle(State(state): State<Arc<AdminState>>, Json(p): Json<Payload>) -> Json<Value> {
    if let Some(id) = p.tool_use_id {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        debug!("pre-tool-use: record {id} at {now}ms");
        state.tool_tracker.record(&id, now);
    } else {
        debug!("pre-tool-use: payload missing tool_use_id, skipped");
    }
    Json(json!({}))
}
