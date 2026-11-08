//! Feature preset of the Codex agent.
//!
//! Tells which features the proxy offers for the Codex ACP agent. Its
//! agent messages are the same as those of the Claude agent.

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
        // Not in the list: the session index. Its requests have no session
        // id, and each session has its own agent.
        list.extend(names(&["air:rawInputRendering", "air:planContentDelta"]));
        list
    })
}
