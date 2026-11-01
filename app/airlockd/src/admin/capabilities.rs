//! Sandbox capabilities that the host enables at boot.
//!
//! Tells which agent features the admin service offers: temporary network
//! grants and network rule editing. The two features are independent.

/// Agent features that the host enabled for this sandbox.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// The host accepts temporary network grants (`execute` tool).
    pub grants: bool,
    /// The rules that the agent can see and change.
    pub rule_editing: RuleEditing,
}

/// Value of `network.rule_editing` on the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuleEditing {
    /// The agent cannot see or change rules.
    #[default]
    None,
    /// The agent can see and change only on-demand rules.
    OnDemand,
    /// The agent can see and change all rules.
    All,
}
