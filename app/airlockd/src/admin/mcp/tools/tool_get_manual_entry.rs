//! The `get_manual_entry` tool: get one page of the user manual as text.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use super::{AdminTools, tool_error};
use crate::admin::mcp::manual;

/// Description of the `get_manual_entry` tool for the agent.
const GET_MANUAL_ENTRY: &str = "Get a page of the airlock user manual.";

/// Arguments of `get_manual_entry`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ManualEntryArgs {
    #[schemars(description = "Page name from `list_manual_entries`.")]
    pub file: String,
}

#[tool_router(router = get_manual_entry_router, vis = "pub(super)")]
impl AdminTools {
    /// Get one page of the user manual.
    #[tool(name = "get_manual_entry", description = GET_MANUAL_ENTRY)]
    async fn get_manual_entry(
        &self,
        Parameters(args): Parameters<ManualEntryArgs>,
    ) -> CallToolResult {
        match manual::page(&args.file) {
            Some(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            None => tool_error(&format!(
                "No manual entry `{}`. Use `list_manual_entries` for the names.",
                args.file
            )),
        }
    }
}
