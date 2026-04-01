mod resizes;

pub use resizes::resizes;

pub struct TerminalGuard {
    raw_mode_enabled: bool,
}

impl TerminalGuard {
    pub fn enter() -> Self {
        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        let raw_mode_enabled = if is_tty {
            crossterm::terminal::enable_raw_mode().is_ok()
        } else {
            false
        };
        Self { raw_mode_enabled }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.raw_mode_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
        }
    }
}
