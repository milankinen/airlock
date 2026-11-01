//! Blocked hosts of a tool call.
//!
//! Ends the time window of a tool call and finds the hosts that the network
//! policy blocked in the window. Makes the text that tells the agent these
//! hosts as the probable cause of a failure. The text tells apart the hosts
//! that a temporary grant can allow and the hosts that it cannot allow (deny
//! rules, local addresses). The hooks of all agents use the same text.

use std::fmt::Write as _;

use tracing::debug;

use crate::admin::deny_tracker::blocked_targets;
use crate::admin::now_ms;
use crate::admin::state::AdminState;

/// Maximum number of blocked hosts in the context.
const MAX_HOSTS: usize = 20;

/// Record the end of a tool call and look for denies in its time window.
/// The first call stores the end time. Later calls use the same window.
/// Args:
///  - `state`: Shared admin state
///  - `key`: Key of the tool call in the tool tracker
///  - `hook`: Hook name for the logs
///
/// Returns:
///   Context text with the blocked hosts, or `None` if the call has no
///   start record or the policy blocked no host in its window.
pub(crate) fn tool_end_context(state: &AdminState, key: &str, hook: &str) -> Option<String> {
    let now = now_ms();
    let Some(window) = state.tool_tracker.finish(key, now) else {
        debug!("{hook}: {key} has no start record, passing through");
        return None;
    };
    // The first hook ends the window now. Denies that the host reports
    // later than this hook are not visible here.
    let denies = state
        .deny_tracker
        .between(window.start, window.deny_end(now));
    // Only hosts with safe names go to the agent. Socket paths have none.
    let (grantable, fixed): (Vec<_>, Vec<_>) =
        denies.into_iter().partition(|entry| entry.grantable);
    let fixed = blocked_targets(&fixed);
    // A host that a deny rule blocked once stays in the "cannot allow" list.
    let targets: Vec<String> = blocked_targets(&grantable)
        .into_iter()
        .filter(|t| !fixed.contains(t))
        .collect();
    if targets.is_empty() && fixed.is_empty() {
        debug!("{hook}: {key} — no blocked host in the tool window, passing through");
        return None;
    }
    debug!(
        "{hook}: {key} — {} grantable and {} fixed blocked host(s), injecting context",
        targets.len(),
        fixed.len()
    );
    Some(blocked_context(&targets, &fixed))
}

/// Make the context that tells the agent the blocked hosts.
/// Args:
///  - `targets`: Blocked `host:port` targets that a temporary grant can
///    allow, with no duplicates
///  - `fixed`: Blocked targets that no grant can allow, with no duplicates
fn blocked_context(targets: &[String], fixed: &[String]) -> String {
    let mut text = String::new();
    if !targets.is_empty() {
        text.push_str(
            "The tool made network calls that the sandbox network policy blocked. Blocked hosts:",
        );
        push_hosts(&mut text, targets);
    }
    if !fixed.is_empty() {
        if text.is_empty() {
            text.push_str("The tool made network calls that the sandbox does not allow at all ");
        } else {
            text.push_str("\nThe sandbox does not allow these hosts at all ");
        }
        text.push_str("(deny rules or local addresses):");
        push_hosts(&mut text, fixed);
        text.push_str("\nDo not request access to these hosts. Ask the user for more details.");
    }
    text
}

/// Add one line for each target, at most `MAX_HOSTS` lines and a count of
/// the others.
fn push_hosts(text: &mut String, targets: &[String]) {
    for t in targets.iter().take(MAX_HOSTS) {
        let _ = write!(text, "\n- {t}");
    }
    if targets.len() > MAX_HOSTS {
        let _ = write!(text, "\n- and {} more", targets.len() - MAX_HOSTS);
    }
}
