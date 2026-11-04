//! Feature preset of the Copilot agent.
//!
//! Tells which features the proxy offers for the Copilot agent in its ACP
//! mode.

use super::{command_name, names};

/// Features of the Copilot agent preset.
pub(super) const FEATURES: &[&str] = &[
    "loadSession",
    "promptCapabilities.image",
    "promptCapabilities.embeddedContext",
    "sessionCapabilities.list",
    "sessionCapabilities.close",
];

/// Get the preset of the Copilot agent.
/// Args:
///  - `argv`: Agent command and its arguments
///
/// Returns:
///   The feature names, or `None` if the command is not Copilot with the
///   `--acp` argument.
pub(super) fn preset(argv: &[String]) -> Option<Vec<String>> {
    (command_name(argv) == "copilot" && argv.iter().skip(1).any(|a| a == "--acp"))
        .then(|| names(FEATURES))
}
