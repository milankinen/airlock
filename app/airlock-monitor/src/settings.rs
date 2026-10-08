//! TUI runtime settings from the `[monitor]` section of the user config.

use crate::keys::KeyBindings;

/// TUI runtime settings from the `[monitor]` section of the user config.
pub struct TuiSettings {
    /// Maximum number of HTTP request entries in the Monitor tab buffer.
    /// When the buffer is full, the oldest entries are removed.
    pub max_http_requests: usize,
    /// Maximum number of TCP connection entries in the Monitor tab buffer.
    pub max_tcp_connections: usize,
    /// Number of scrollback rows that the embedded terminal of the Sandbox
    /// tab keeps.
    pub scrollback: u16,
    /// Map from key to action. The TUI reads it on each keystroke. Airlock
    /// builds it one time at startup from the user's `[monitor.keys]`
    /// config, or from the defaults.
    pub keys: KeyBindings,
}

// The defaults are the same values that the code used before they became
// configurable.
impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            max_http_requests: 100,
            max_tcp_connections: 100,
            scrollback: 1000,
            keys: KeyBindings::defaults(),
        }
    }
}
