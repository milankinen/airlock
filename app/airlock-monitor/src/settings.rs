//! TUI runtime settings. Sourced from the `[monitor]` section of the
//! user config; defaults match what the values used to be hard-coded to.

use crate::keys::KeyBindings;
use crate::mouse::MousePassthrough;

pub struct TuiSettings {
    /// Max HTTP request entries kept in the Monitor tab buffer. Older
    /// entries are dropped once this cap is reached.
    pub max_http_requests: usize,
    /// Max TCP connection entries kept in the Monitor tab buffer.
    pub max_tcp_connections: usize,
    /// Scrollback rows retained by the embedded vt100 terminal that
    /// drives the sandbox tab.
    pub scrollback: u16,
    /// Key → action map consulted on every keystroke. Built once at
    /// startup from the user's `[monitor.keys]` config (or defaults).
    pub keys: KeyBindings,
    /// How much of the mouse the Sandbox tab passes through. Defaults to
    /// `None`, keeping every event in the TUI; `All` re-encodes them into
    /// the guest PTY instead, so mouse wheel scrolling works inside the
    /// sandboxed program.
    pub mouse_passthrough: MousePassthrough,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            max_http_requests: 100,
            max_tcp_connections: 100,
            scrollback: 1000,
            keys: KeyBindings::defaults(),
            mouse_passthrough: MousePassthrough::default(),
        }
    }
}
