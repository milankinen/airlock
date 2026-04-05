use crate::rpc;

pub fn setup() -> Terminal {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    Terminal {
        is_tty,
        guard: None,
    }
}

pub struct Terminal {
    is_tty: bool,
    guard: Option<TerminalGuard>,
}

impl Terminal {
    pub fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Enter raw terminal mode. Call this only when ready for VM interaction
    /// (after downloads complete) so Ctrl+C works during setup.
    pub fn enter_raw_mode(&mut self) {
        if self.is_tty && self.guard.is_none() {
            let raw_mode_enabled = crossterm::terminal::enable_raw_mode().is_ok();
            self.guard = Some(TerminalGuard { raw_mode_enabled });
        }
    }

    pub fn stdin(&self) -> anyhow::Result<rpc::Stdin> {
        let (pty_size, resizes) = if self.is_tty {
            // crossterm::terminal::size() returns (cols, rows)
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

struct TerminalGuard {
    raw_mode_enabled: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.raw_mode_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
        }
    }
}
