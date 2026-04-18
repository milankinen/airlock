//! Application state for the TUI.

use std::sync::Arc;

use crate::NetworkControl;
use crate::tabs::monitor::MonitorTab;

/// Which tab is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Sandbox,
    Monitor,
}

/// Top-level TUI application state.
pub struct App {
    pub active_tab: Tab,
    pub monitor: MonitorTab,
    pub network: Arc<dyn NetworkControl>,
    /// Whether mouse events are captured. When `false`, the host terminal
    /// handles clicks natively (enabling text selection). Toggled with F12.
    pub mouse_captured: bool,
    /// Tracks whether the guest has enabled bracketed paste mode
    /// (`\e[?2004h`). Only when true do we wrap pasted text in
    /// `\e[200~...\e[201~` before forwarding — shells without bracketed
    /// paste support (BusyBox ash etc.) mis-parse the markers and eat
    /// surrounding bytes.
    pub guest_bracketed_paste: bool,
}

impl App {
    pub fn new(network: Arc<dyn NetworkControl>, project_path: String) -> Self {
        Self {
            active_tab: Tab::Sandbox,
            monitor: MonitorTab::new(project_path),
            network,
            mouse_captured: true,
            guest_bracketed_paste: false,
        }
    }
}
