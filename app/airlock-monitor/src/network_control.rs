//! Thread-safe handle the TUI uses to read and mutate live network state.
//!
//! The host side (`airlock::network::NetworkControl`) implements this trait;
//! the TUI holds an `Arc<dyn NetworkControl>` and calls through it when the
//! user flips policy or toggles rules. Keeping the contract here — rather
//! than importing the airlock crate — lets the TUI crate stay a leaf.

use ratatui::style::Color;

/// Top-level network policy, mirrors `airlock::config::Policy` for TUI use.
///
/// The display order is the order rendered in the policy dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Policy {
    AllowAlways,
    AllowByDefault,
    DenyByDefault,
    DenyAlways,
}

impl Policy {
    /// Entries in drop-down order.
    pub const ALL: [Policy; 4] = [
        Policy::AllowAlways,
        Policy::AllowByDefault,
        Policy::DenyByDefault,
        Policy::DenyAlways,
    ];

    /// Kebab-case label — matches the on-disk config form.
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

    /// Accent color used in the policy title. `always` variants flag the
    /// extremes (green / red); the `by-default` variants share blue as the
    /// neutral middle.
    pub fn color(self) -> Color {
        match self {
            Policy::AllowAlways => Color::Green,
            Policy::AllowByDefault | Policy::DenyByDefault => Color::Blue,
            Policy::DenyAlways => Color::Red,
        }
    }
}

/// Host-side control surface used by the TUI. All methods are cheap and
/// lock-protected on the host side.
pub trait NetworkControl: Send + Sync {
    fn policy(&self) -> Policy;
    fn set_policy(&self, policy: Policy);
}
