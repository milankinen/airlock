//! Network control interface for the TUI.
//!
//! Lets the TUI read and change the live network policy of the sandbox. The
//! host implements this interface.

use ratatui::style::Color;

/// Top-level network policy. Copy of `airlock::config::config_values::Policy` for the TUI.
///
/// The variant order is the order in the policy dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Policy {
    /// Allow all connections. Rules have no effect.
    AllowAlways,
    /// Allow connections that no rule denies.
    AllowByDefault,
    /// Deny connections that no rule allows.
    DenyByDefault,
    /// Deny all connections. Rules have no effect.
    DenyAlways,
}

impl Policy {
    /// All policies in dropdown order.
    pub const ALL: [Policy; 4] = [
        Policy::AllowAlways,
        Policy::AllowByDefault,
        Policy::DenyByDefault,
        Policy::DenyAlways,
    ];

    /// Kebab-case label. It is the same as the value in the config file.
    pub fn label(self) -> &'static str {
        match self {
            Policy::AllowAlways => "allow-always",
            Policy::AllowByDefault => "allow-by-default",
            Policy::DenyByDefault => "deny-by-default",
            Policy::DenyAlways => "deny-always",
        }
    }

    /// Human-readable label used in the TUI (title bar, dropdown rows).
    pub fn title(self) -> &'static str {
        match self {
            Policy::AllowAlways => "Always allow",
            Policy::AllowByDefault => "Allow by default",
            Policy::DenyByDefault => "Deny by default",
            Policy::DenyAlways => "Always deny",
        }
    }

    /// Accent color for the policy title.
    ///
    /// The `always` variants are the extremes and use green or red. The
    /// `by-default` variants are the neutral middle and both use cyan.
    pub fn color(self) -> Color {
        match self {
            Policy::AllowAlways => Color::Green,
            Policy::AllowByDefault | Policy::DenyByDefault => Color::Cyan,
            Policy::DenyAlways => Color::Red,
        }
    }
}

/// Host-side network control that the TUI uses. All methods are cheap.
/// The host side protects them with a lock.
///
/// The host side (`airlock::network::NetworkControl`) implements the trait.
/// The trait is in this crate, so this crate does not depend on the
/// airlock crate.
pub trait NetworkControl: Send + Sync {
    /// Current network policy.
    fn policy(&self) -> Policy;
    /// Change the network policy to `policy`.
    fn set_policy(&self, policy: Policy);
}
