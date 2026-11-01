//! Instructions for the agent about the `airlock` tools.
//!
//! Makes the text that tells the agent which airlock tools it has and when
//! to use them. The text describes only the features that the host enabled.
//! The MCP server and the session hooks of the agents give the same text.

use crate::admin::capabilities::{Capabilities, RuleEditing};

/// Make the agent instructions for the enabled features.
/// Args:
///  - `caps`: Agent features that the host enabled
///  - `prefix`: Text before each tool name, for example `mcp__airlock__`
///    for Claude Code and Codex, `airlock-` for Copilot
pub(crate) fn instructions(caps: Capabilities, prefix: &str) -> String {
    let tool = |name: &str| format!("`{prefix}{name}`");
    let (execute, show, upsert) = (tool("execute"), tool("show_rules"), tool("upsert_rule"));
    let rules = caps.rule_editing != RuleEditing::None;
    let intro = "You run inside an airlock sandbox. If its network policy blocks network \
                 calls during a task";
    let mut lines = match (caps.grants, rules) {
        (true, true) => vec![
            format!("{intro}, decide if the task is ad hoc or repeats:"),
            format!(
                "- Ad hoc: run the command with the {execute} tool and give the blocked hosts."
            ),
            "- Many or unknown hosts: ask the user, then give `hosts: [\"*\"]`.".into(),
            format!(
                "- Repeats: list rules with {show}, then run the command with {execute} and a \
                 matching rule. If no rule matches, give the hosts, then use the `hosts` of the \
                 result to create or change a rule with {upsert}."
            ),
        ],
        (true, false) => vec![format!(
            "{intro}, run the command with the {execute} tool and give the blocked hosts. For \
             many or unknown hosts, ask the user, then give `hosts: [\"*\"]`."
        )],
        (false, true) => vec![format!(
            "{intro}, list rules with {show}, then create or change a rule with {upsert}."
        )],
        (false, false) => vec![format!(
            "{intro}, you cannot change the policy. Ask the user."
        )],
    };
    if caps.rule_editing == RuleEditing::OnDemand {
        lines.push("You can see and change only on-demand rules.".into());
    }
    let action = match (caps.grants, rules) {
        (true, true) => Some("run a command or change a rule"),
        (true, false) => Some("run a command"),
        (false, true) => Some("change a rule"),
        (false, false) => None,
    };
    if let Some(action) = action {
        lines.push(format!(
            "Before you {action}, tell the user what you do and why. The user approves each \
             request on the host and sees the hosts and rules there, so do not repeat them."
        ));
    }
    lines.push(format!(
        "For questions about airlock, read its user manual with {} and {}.",
        tool("list_manual_entries"),
        tool("get_manual_entry")
    ));
    lines.join("\n")
}
