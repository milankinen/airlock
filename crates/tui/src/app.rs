//! Application state for the TUI.

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
    pub policy: String,
    /// Whether mouse events are captured. When `false`, the host terminal
    /// handles clicks natively (enabling text selection). Toggled with F12.
    pub mouse_captured: bool,
}

impl App {
    pub fn new(policy: String, project_path: String) -> Self {
        Self {
            active_tab: Tab::Sandbox,
            monitor: MonitorTab::new(project_path),
            policy,
            mouse_captured: true,
        }
    }
}
