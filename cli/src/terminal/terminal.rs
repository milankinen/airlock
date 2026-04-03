use crate::project::Project;
use crate::rpc;

pub fn setup(project: &Project) -> anyhow::Result<Terminal> {
    if !project.config.terminal {
        return Ok(Terminal { guard: None })
    }
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let raw_mode_enabled = if is_tty {
        crossterm::terminal::enable_raw_mode().is_ok()
    } else {
        false
    };
    Ok(Terminal {
        guard: Some( TerminalGuard { raw_mode_enabled })
    })

}

pub struct Terminal {
    guard: Option<TerminalGuard>
}

impl Terminal {
    pub fn stdin(&self) -> anyhow::Result<rpc::Stdin> {
        let (pty_size, resizes) = match &self.guard {
            Some(_) => {
                let pty_size = crossterm::terminal::size().unwrap_or((80, 24));
                let resizes = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::window_change()
                )?;
                (Some(pty_size), Some(resizes))
            },
            None => (None, None)
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
