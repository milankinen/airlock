//! Application state for the TUI.

use crate::tabs::network::NetworkTab;

/// Which tab is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Sandbox,
    Network,
}

/// Top-level TUI application state.
pub struct App {
    pub active_tab: Tab,
    pub network: NetworkTab,
    pub policy: String,
    /// Whether mouse events are captured. When `false`, the host terminal
    /// handles clicks natively (enabling text selection). Toggled with F12.
    pub mouse_captured: bool,
}

impl App {
    pub fn new(policy: String) -> Self {
        Self {
            active_tab: Tab::Sandbox,
            network: NetworkTab::new(),
            policy,
            mouse_captured: true,
        }
    }
}
