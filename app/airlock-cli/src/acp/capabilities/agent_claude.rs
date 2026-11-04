//! Feature preset of the Claude agent.
//!
//! Tells which features the proxy offers for the Claude ACP agent. The
//! Codex agent shares most of these features. Also tells the agent
//! messages that only this agent sends, and the client feature that only
//! this agent reads.

use serde_json::{Map, Value};

use super::{command_name, names};

/// Features of the Claude agent preset, without the agent specific AIR
/// features.
pub(super) const FEATURES: &[&str] = &[
    "loadSession",
    "promptCapabilities.image",
    "promptCapabilities.embeddedContext",
    "sessionCapabilities.list",
    "sessionCapabilities.resume",
    "sessionCapabilities.close",
    "sessionCapabilities.delete",
    "sessionCapabilities.fork",
    "sessionCapabilities.additionalDirectories",
    "sessionCapabilities.systemPrompt",
    "sessionCapabilities.subagents",
    "_meta.authStatus",
    "steering",
    "air:goal",
    "air:sessionFailure",
    "air:nativeSubagentSessions",
    "air:asyncTasks",
    "air:customInstructions",
    "air:recommendedValue",
    "air:diffPatch",
];

/// Get the preset of the Claude agent.
/// Args:
///  - `argv`: Agent command and its arguments
///
/// Returns:
///   The feature names, or `None` if the command is not the Claude agent.
pub(super) fn preset(argv: &[String]) -> Option<Vec<String>> {
    (command_name(argv) == "claude-agent-acp").then(|| {
        let mut list = names(FEATURES);
        list.extend(names(&["air:backgroundSubagents", "air:sessionIndex"]));
        list
    })
}

/// Agent notifications of this agent that the client gets in restrict
/// mode.
// The login status is data for the display only.
pub(super) const AGENT_NOTIFICATIONS: &[&str] = &["_auth/status_update"];

/// Client `_meta` feature that this agent reads and that is safe in
/// restrict mode.
// The sub-agent transcript makes the agent put the sub-agent text in the
// parent session.
const META_NAMES: &[&str] = &["subagent-transcript"];

/// Copy the safe features that this agent reads from the client `_meta` to
/// the kept `_meta`.
/// Args:
///  - `meta`: Client `_meta` features
///
/// Returns:
///   The kept `_meta` features.
pub(super) fn keep_meta(meta: &Map<String, Value>) -> Map<String, Value> {
    let mut kept = Map::new();
    for name in META_NAMES {
        if let Some(value) = meta.get(*name) {
            kept.insert((*name).into(), value.clone());
        }
    }
    kept
}
