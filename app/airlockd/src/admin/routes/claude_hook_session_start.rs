//! Hook at the start of a session.
//!
//! Claude Code calls this hook when a session starts, resumes, clears or
//! compacts. Codex calls it when a session starts. The hook gives the agent
//! the instructions for the airlock tools. Both agents use the same output
//! format and the same MCP tool names. The instructions describe only the
//! features that the host enabled.

use axum::Json;
use serde_json::{Value, json};

use crate::admin::capabilities::Capabilities;
use crate::admin::mcp::instructions;

/// Give the airlock tool instructions as additional session context.
/// Args:
///  - `caps`: Agent features that the host enabled
pub fn handle(caps: Capabilities) -> Json<Value> {
    Json(json!({ "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": instructions(caps, "mcp__airlock__"),
    }}))
}
