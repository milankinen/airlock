//! GitHub Copilot CLI hooks.
//!
//! Copilot calls these hooks when a session starts and before and after each
//! tool call. The session hook gives Copilot the instructions for the
//! airlock tools. The tool hooks record the time window of each tool call.
//! After a failed call, they tell Copilot the hosts that the network policy
//! blocked while the tool ran.
//!
//! Copilot gives no ID for a tool call. The hooks identify a call by its
//! session, tool name and tool arguments. Copilot reads a flat output
//! object, with no event name.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::admin::blocked_hosts::tool_end_context;
use crate::admin::capabilities::Capabilities;
use crate::admin::mcp::instructions;
use crate::admin::now_ms;
use crate::admin::state::AdminState;

/// Hook request body of the tool hooks. Only the used fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    session_id: Option<String>,
    tool_name: Option<String>,
    /// JSON object, or a string that contains the JSON object.
    #[serde(default)]
    tool_args: Value,
}

impl Payload {
    /// Get the key of the tool call in the tool tracker.
    ///
    /// Returns:
    ///   Key text, or `None` if the session ID or the tool name is missing.
    fn key(&self) -> Option<String> {
        let (session, tool) = (self.session_id.as_ref()?, self.tool_name.as_ref()?);
        // The pre and post hooks can give the arguments in different forms,
        // so a JSON string is parsed and object keys are sorted.
        let args = match &self.tool_args {
            Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
            other => other.clone(),
        };
        // A hash keeps large arguments (for example file contents) out of
        // the tracker. A hash collision only mixes up the time windows of
        // two calls.
        let mut hasher = DefaultHasher::new();
        (session, tool, canonical(&args).to_string()).hash(&mut hasher);
        Some(format!("copilot:{:016x}", hasher.finish()))
    }
}

/// Copy a JSON value with the keys of all objects in sorted order.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let sorted: Map<String, Value> = keys
                .into_iter()
                .map(|k| (k.clone(), canonical(&map[k])))
                .collect();
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// Give the airlock tool instructions as additional session context.
/// Args:
///  - `caps`: Agent features that the host enabled
pub fn session_start(caps: Capabilities) -> Json<Value> {
    // Copilot shows the MCP tools to the model as `<server>-<tool>`.
    Json(json!({ "additionalContext": instructions(caps, "airlock-") }))
}

/// Record the current time as the start of the tool call. Always returns an
/// empty JSON object, so the hook does not change the tool behavior.
pub async fn pre_tool_use(
    State(state): State<Arc<AdminState>>,
    Json(p): Json<Payload>,
) -> Json<Value> {
    if let Some(key) = p.key() {
        let now = now_ms();
        debug!("copilot pre-tool-use: record {key} at {now}ms");
        state.tool_tracker.record(&key, now);
    } else {
        debug!("copilot pre-tool-use: payload missing sessionId or toolName, skipped");
    }
    Json(json!({}))
}

/// Record the end of the tool call and keep the record. Always returns an
/// empty JSON object.
pub async fn post_tool_use(
    State(state): State<Arc<AdminState>>,
    Json(p): Json<Payload>,
) -> Json<Value> {
    if let Some(key) = p.key() {
        let found = state.tool_tracker.finish(&key, now_ms()).is_some();
        debug!("copilot post-tool-use: {key} (record found: {found})");
    } else {
        debug!("copilot post-tool-use: payload missing sessionId or toolName, skipped");
    }
    Json(json!({}))
}

/// Record the end of the failed tool call and look for denies in its time
/// window. Duplicate hooks use the same time window.
///
/// Returns:
///   If the host blocked hosts while the tool ran, an object with the
///   blocked hosts in `additionalContext`. Else an empty JSON object.
pub async fn post_tool_use_failure(
    State(state): State<Arc<AdminState>>,
    Json(p): Json<Payload>,
) -> Json<Value> {
    let Some(key) = p.key() else {
        debug!(
            "copilot post-tool-use-failure: payload missing sessionId or toolName, passing through"
        );
        return Json(json!({}));
    };
    match tool_end_context(&state, &key, "copilot post-tool-use-failure") {
        Some(context) => Json(json!({ "additionalContext": context })),
        None => Json(json!({})),
    }
}
