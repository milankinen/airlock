//! `POST /claude/hooks/pre-tool-use` — record the tool call's start time.
//!
//! Claude Code fires this hook before it invokes a tool. We store
//! `tool_use_id → now` so the matching post-tool-use-failure route can
//! compare against the `DenyTracker` and decide whether a failure was
//! caused by a policy deny. Always responds with an empty object so the
//! hook passes through without modifying tool behavior.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

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
