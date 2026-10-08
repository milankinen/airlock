//! Raw terminal runtime.
//!
//! Connects the guest directly to the user's terminal, without the monitor.
//! Guest output goes directly to the host stdout and stderr.

use std::io::Write;

use airlock_common::supervisor_capnp::stdin;

use super::{OutputSink, PtySize, Runtime, SignalStream, Terminal};
use crate::network::NetworkHandle;
use crate::project::Project;
use crate::rpc;

/// Runtime that controls raw mode on the host terminal and gives the stdin
/// and resize event sources.
pub struct RawTerminalRuntime {
    is_tty: bool,
    guard: Option<TerminalGuard>,
}

impl RawTerminalRuntime {
    /// Make a runtime. Checks if stdin is a TTY.
    pub fn new() -> Self {
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        Self {
            is_tty,
            guard: None,
        }
    }

    /// Check if stdin is a terminal.
    #[allow(dead_code)]
    pub fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Enter raw terminal mode. Call this only when the VM interaction
    /// starts (after the downloads), so Ctrl+C works during setup.
    fn enter_raw_mode(&mut self) {
        // Enable xterm `modifyOtherKeys` level 1. The host terminal then
        // encodes Shift+Enter as `\e[27;2;13~`, not as Enter `\r`. Level 1
        // keeps Ctrl+C as `0x03` for the PTY line discipline. The guest app
        // can identify Shift+Enter without a kitty protocol negotiation.
        //
        // Do not force bracketed paste mode. The guest shell enables it with
        // its own `\e[?2004h`. Shells without support (BusyBox ash) then get
        // raw bytes, as in a normal terminal, and no markers that they parse
        // incorrectly.
        if self.is_tty && self.guard.is_none() {
            let raw_mode_enabled = crossterm::terminal::enable_raw_mode().is_ok();
            let modify_other_keys =
                std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[>4;1m").is_ok();
            self.guard = Some(TerminalGuard {
                raw_mode_enabled,
                modify_other_keys,
            });
        }
    }

    /// Enter raw mode (see [`enter_raw_mode`](Self::enter_raw_mode)) and
    /// change the runtime into the output sink. The sink owns the terminal
    /// until it drops.
    pub fn into_terminal(mut self) -> RawTerminal {
        self.enter_raw_mode();
        RawTerminal { _guard: self.guard }
    }

    /// Make an RPC stdin server. On a TTY, it also sends resize events.
    pub fn stdin(&self) -> anyhow::Result<rpc::Stdin> {
        let (pty_size, resizes) = if self.is_tty {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            tracing::debug!("host terminal size: {rows}x{cols}");
            let pty_size = (rows, cols);
            let resizes =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
            (Some(pty_size), Some(resizes))
        } else {
            (None, None)
        };
        Ok(rpc::Stdin::new(tokio::io::stdin(), pty_size, resizes))
    }
}

impl Default for RawTerminalRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime for RawTerminalRuntime {
    type Terminal = RawTerminal;

    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)> {
        let stdin = self.stdin()?;
        let pty_size = stdin.pty_size();
        Ok((capnp_rpc::new_client(stdin), pty_size))
    }

    fn signals(&mut self) -> anyhow::Result<SignalStream> {
        super::signals()
    }

    fn launch(
        self,
        _project: &Project,
        _network: &NetworkHandle,
        _supervisor: rpc::Supervisor,
    ) -> anyhow::Result<RawTerminal> {
        Ok(self.into_terminal())
    }
}

/// Output sink that writes guest bytes directly to the host stdout and
/// stderr.
pub struct RawTerminal {
    /// Kept for its `Drop` impl: restores cooked mode and `modifyOtherKeys`
    /// when the sandbox exits.
    _guard: Option<TerminalGuard>,
}

impl OutputSink for RawTerminal {
    fn stdout(&mut self, bytes: &[u8]) {
        let _ = std::io::stdout().write_all(bytes);
        let _ = std::io::stdout().flush();
    }

    fn stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().write_all(bytes);
        let _ = std::io::stderr().flush();
    }
}

impl Terminal for RawTerminal {
    fn exit(self, exit_code: i32) -> i32 {
        exit_code
    }
}

/// RAII guard that restores the cooked terminal mode on drop.
struct TerminalGuard {
    raw_mode_enabled: bool,
    modify_other_keys: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.modify_other_keys {
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[>4;0m");
        }
        if self.raw_mode_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
        }
    }
}
