//! Admin service for sandbox tools.
//!
//! Runs an HTTP service at `http://admin.airlock/`. Only processes inside the
//! VM can access it. Sandbox tools use it to work with the network policy of
//! the host. The most important users are the hooks of Claude Code, Codex
//! and GitHub Copilot CLI. The hooks tell the agent when a network deny is
//! the probable cause of a failed tool call. The `airlock` MCP server lets
//! an agent run a command with extended network access and manage
//! on-demand network rules. The user approves each request.

pub mod blocked_hosts;
pub mod capabilities;
pub mod deny_tracker;
pub mod mcp;
pub mod routes;
pub mod server;
pub mod state;
pub mod tool_tracker;

#[cfg(test)]
mod tests;

pub use capabilities::{Capabilities, RuleEditing};
pub use deny_tracker::{DenyEntry, DenyTracker};
pub use mcp::{GrantContext, SpawnCreds};
pub use server::start;
pub use state::AdminState;

/// Get the current time as Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
