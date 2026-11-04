//! Feature preset of the Codex agent.
//!
//! Tells which features the proxy offers for the Codex ACP agent, and the
//! agent messages that only this agent sends.

use super::agent_claude::FEATURES;
use super::{command_name, names};

/// Get the preset of the Codex agent.
/// Args:
///  - `argv`: Agent command and its arguments
///
/// Returns:
///   The feature names, or `None` if the command is not the Codex agent.
pub(super) fn preset(argv: &[String]) -> Option<Vec<String>> {
    (command_name(argv) == "codex-acp").then(|| {
        let mut list = names(FEATURES);
        list.extend(names(&[
            "air:sessionIndex",
            "air:rawInputRendering",
            "air:planContentDelta",
        ]));
        list
    })
}

/// Agent notifications of this agent that the client gets in restrict
/// mode.
// The Codex adapter sends the login status too. It is data for the display
// only.
pub(super) const AGENT_NOTIFICATIONS: &[&str] = &["_auth/status_update"];
