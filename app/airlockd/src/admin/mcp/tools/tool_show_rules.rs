//! The `show_rules` tool: show the network rules that the sandbox can see.

use rmcp::handler::server::wrapper::Json;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router};
use serde::Serialize;

use super::{AdminTools, tool_error};
use crate::admin::mcp::bridge::Rule;

/// Description of the `show_rules` tool for the agent.
const SHOW_RULES: &str = "Show the sandbox network rules: name, enabled (true, false or \
\"on-demand\"), description, allow and deny.";

/// Result of `show_rules`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RuleList {
    /// Rules that the sandbox can see.
    pub rules: Vec<Rule>,
}

#[tool_router(router = show_rules_router, vis = "pub(super)")]
impl AdminTools {
    /// Show the network rules that the sandbox can see.
    #[tool(name = "show_rules", description = SHOW_RULES)]
    async fn show_rules(&self) -> Result<Json<RuleList>, CallToolResult> {
        let rules = self
            .grants()?
            .list_rules()
            .await
            .map_err(|e| tool_error(&e))?;
        Ok(Json(RuleList { rules }))
    }
}
