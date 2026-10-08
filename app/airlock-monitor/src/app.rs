//! Application state for the TUI.

use std::sync::Arc;

use crate::NetworkControl;
use crate::settings::TuiSettings;
use crate::tabs::monitor::MonitorTab;

/// TUI tab that the user can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// Embedded terminal of the sandbox process.
    Sandbox,
    /// Resource usage and network activity of the sandbox.
    Monitor,
}

/// Top-level TUI application state.
pub struct App {
    /// Tab that is visible now.
    pub active_tab: Tab,
    /// State of the Monitor tab.
    pub monitor: MonitorTab,
    /// Handle to read and change the live network policy.
    pub network: Arc<dyn NetworkControl>,
    /// Time of the last left click, or `None` if the user did not click yet.
    ///
    /// The TUI holds the terminal's mouse capture for the whole session.
    /// Thus a plain drag never selects text. A click often means that the
    /// user tries to select text. Thus the status line briefly shows the
    /// modifier that bypasses the capture in this terminal. See
    /// `ui::build_status_line`.
    pub select_hint_at: Option<std::time::Instant>,
    /// True when the guest enabled bracketed paste mode (`\e[?2004h`).
    ///
    /// Pasted text gets the `\e[200~...\e[201~` markers only when this is
    /// true. Shells without bracketed paste support (for example BusyBox
    /// ash) parse the markers incorrectly and discard the bytes near them.
    pub guest_bracketed_paste: bool,
    /// Runtime settings from the user config.
    pub settings: TuiSettings,
}

impl App {
    /// Create the initial application state, with the Sandbox tab active.
    /// Args:
    ///  - `network`: Handle to the live network policy
    ///  - `project_path`: Project path to show on the Monitor tab
    ///  - `version`: Airlock version to show on the Monitor tab
    ///  - `settings`: Runtime settings from the user config.
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
