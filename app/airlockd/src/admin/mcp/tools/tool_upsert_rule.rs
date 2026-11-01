//! The `upsert_rule` tool: ask the user to add or change a network rule.

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{AdminTools, tool_error};
use crate::admin::mcp::bridge::RuleChange;

/// Description of the `upsert_rule` tool for the agent.
const UPSERT_RULE: &str = "\
Add or change a network rule. Give only the values to change. The user \
approves the change. See the `configuration/network.md` manual entry.";

// The schema texts are on one line. A wrapped doc comment puts a line
// break into the schema that the agent sees.

/// Arguments of `upsert_rule`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpsertRuleArgs {
    #[schemars(description = "Name of the rule: a-z, 0-9, `_` and `-`.")]
    pub name: String,
    #[schemars(
        description = "`true` (always on), `false` (off) or `\"on-demand\"` (only for `execute` with `rule`). Default: keep, or `\"on-demand\"` for a new rule."
    )]
    #[serde(default)]
    pub enabled: Option<EnabledArg>,
    #[schemars(description = "Short description of the rule for the user.")]
    #[serde(default)]
    pub description: Option<String>,
    #[schemars(
        description = "Full list of host patterns to allow, as `host` or `host:port` (`*.suffix` allowed). Necessary for a new rule."
    )]
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[schemars(description = "Full list of host patterns to deny, in the same format as `allow`.")]
    #[serde(default)]
    pub deny: Option<Vec<String>>,
}

/// Value of the `enabled` argument: a boolean or `"on-demand"`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum EnabledArg {
    /// `true` or `false`.
    Bool(bool),
    /// `"on-demand"`.
    Text(String),
}

/// Result of `upsert_rule`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UpsertOutput {
    /// True if the user approved the change.
    pub approved: bool,
}

#[tool_router(router = upsert_rule_router, vis = "pub(super)")]
impl AdminTools {
    /// Ask the user to add or change a network rule.
    #[tool(name = "upsert_rule", description = UPSERT_RULE)]
    async fn upsert_rule(
        &self,
        Parameters(args): Parameters<UpsertRuleArgs>,
        ct: CancellationToken,
    ) -> Result<Json<UpsertOutput>, CallToolResult> {
        let grants = self.grants()?;
        let trim = |list: Vec<String>| -> Vec<String> {
            list.iter()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .collect()
        };
        let enabled = match args.enabled {
            None => None,
            Some(EnabledArg::Bool(on)) => Some(on.to_string()),
            Some(EnabledArg::Text(text)) if text == "on-demand" => Some(text),
            Some(EnabledArg::Text(_)) => {
                return Err(tool_error(
                    "The `enabled` value must be true, false or \"on-demand\".",
                ));
            }
        };
        let rule = RuleChange {
            name: args.name.trim().to_string(),
            enabled,
            description: args.description,
            allow: args.allow.map(trim),
            deny: args.deny.map(trim),
        };
        if rule.name.is_empty() {
            return Err(tool_error("The `name` is empty."));
        }
        let result = tokio::select! {
            result = grants.upsert_rule(rule) => result,
            () = ct.cancelled() => Err("The request was cancelled.".into()),
        };
        result
            .map(|approved| Json(UpsertOutput { approved }))
            .map_err(|e| tool_error(&e))
    }
}
