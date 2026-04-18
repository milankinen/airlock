//! Non-monitor `Runtime`: writes guest output straight through to the host's
//! real stdout/stderr and manages raw mode on the host terminal.

use std::io::Write;

use airlock_protocol::supervisor_capnp::stdin;

use super::{PtySize, Runtime, SignalStream, Terminal};
use crate::network::Network;
use crate::project::Project;
use crate::rpc;

/// Manages raw mode entry/exit and provides stdin/resize event sources.
pub struct RawTerminalRuntime {
    is_tty: bool,
    guard: Option<TerminalGuard>,
}

impl RawTerminalRuntime {
    /// Detect whether stdin is a TTY and build a handle.
    pub fn new() -> Self {
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        Self {
            is_tty,
            guard: None,
        }
    }

    /// Returns true if stdin is a terminal.
    #[allow(dead_code)]
    pub fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Enter raw terminal mode. Call this only when ready for VM interaction
    /// (after downloads complete) so Ctrl+C works during setup.
    ///
    /// Enables xterm `modifyOtherKeys` level 1 so the host terminal encodes
    /// Shift+Enter as `\e[27;2;13~` (distinct from bare Enter `\r`). Level 1
    /// leaves keys with well-known behavior alone, so Ctrl+C stays `0x03` for
    /// PTY line discipline. The guest app sees a distinguishable Shift+Enter
    /// without needing to negotiate the kitty protocol through the pipe.
    pub fn enter_raw_mode(&mut self) {
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

    /// Create an RPC stdin server, optionally with resize events if TTY.
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

    fn signals(&self) -> anyhow::Result<SignalStream> {
        super::signals()
    }

    fn launch(
        mut self,
        _project: &Project,
        _network: &Network,
        _supervisor: rpc::Supervisor,
    ) -> anyhow::Result<RawTerminal> {
        self.enter_raw_mode();
        Ok(RawTerminal { _guard: self.guard })
    }
}

/// Output sink: writes guest bytes directly to the host stdout/stderr.
pub struct RawTerminal {
    /// Kept for its `Drop` impl: restores cooked mode and `modifyOtherKeys`
    /// when the sandbox exits.
    _guard: Option<TerminalGuard>,
}

impl Terminal for RawTerminal {
    fn stdout(&mut self, bytes: &[u8]) {
        let _ = std::io::stdout().write_all(bytes);
        let _ = std::io::stdout().flush();
    }

    fn stderr(&mut self, bytes: &[u8]) {
        let _ = std::io::stderr().write_all(bytes);
        let _ = std::io::stderr().flush();
    }

    fn exit(self, exit_code: i32) -> i32 {
        exit_code
    }
}

/// RAII guard that restores the terminal to cooked mode on drop.
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
