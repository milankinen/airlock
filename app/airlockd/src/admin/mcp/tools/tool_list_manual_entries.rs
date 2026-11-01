//! The `list_manual_entries` tool: list the pages of the user manual.

use rmcp::handler::server::wrapper::Json;
use rmcp::{schemars, tool, tool_router};
use serde::Serialize;

use super::AdminTools;
use crate::admin::mcp::manual;

/// Description of the `list_manual_entries` tool for the agent.
const LIST_MANUAL_ENTRIES: &str = "List the pages of the airlock user manual.";

/// Result of `list_manual_entries`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ManualEntries {
    /// Page names for `get_manual_entry`.
    pub entries: Vec<&'static str>,
}

#[tool_router(router = list_manual_entries_router, vis = "pub(super)")]
impl AdminTools {
    /// List the pages of the user manual.
    #[tool(name = "list_manual_entries", description = LIST_MANUAL_ENTRIES)]
    async fn list_manual_entries(&self) -> Json<ManualEntries> {
        Json(ManualEntries {
            entries: manual::names(),
        })
    }
}
