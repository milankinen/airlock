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
}

impl App {
    pub fn new(network: Arc<dyn NetworkControl>, project_path: String) -> Self {
        Self {
            active_tab: Tab::Sandbox,
            monitor: MonitorTab::new(project_path),
            network,
            mouse_captured: true,
        }
    }
}
