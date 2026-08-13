//! Application state for the TUI.

use std::sync::Arc;

use crate::NetworkControl;
use crate::settings::TuiSettings;
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
    /// When the user last left-clicked, or `None` if they haven't yet.
    ///
    /// The TUI holds the terminal's mouse capture for the whole session,
    /// so a plain drag never selects text. A click is the moment someone
    /// is reaching for a selection, so it briefly surfaces the modifier
    /// their terminal uses to bypass capture — see `ui::build_status_line`.
    pub select_hint_at: Option<std::time::Instant>,
    /// Tracks whether the guest has enabled bracketed paste mode
    /// (`\e[?2004h`). Only when true do we wrap pasted text in
    /// `\e[200~...\e[201~` before forwarding — shells without bracketed
    /// paste support (BusyBox ash etc.) mis-parse the markers and eat
    /// surrounding bytes.
    pub guest_bracketed_paste: bool,
    pub settings: TuiSettings,
}

impl App {
    pub fn new(
        network: Arc<dyn NetworkControl>,
        project_path: String,
        version: String,
        settings: TuiSettings,
    ) -> Self {
        Self {
            active_tab: Tab::Sandbox,
            monitor: MonitorTab::new(project_path, version),
            network,
            select_hint_at: None,
            guest_bracketed_paste: false,
            settings,
        }
    }
}
