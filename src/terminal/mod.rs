use crate::error::{Error, Result};
use std::os::unix::io::{FromRawFd, RawFd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
            // Print a newline so the shell prompt starts on a fresh line
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\r\n");
        }
    }
}

/// Run bidirectional relay between the user's terminal and the VM console.
///
/// - stdin bytes are forwarded to `write_fd` (goes to guest)
/// - bytes from `read_fd` (from guest) are written to stdout
///
/// Returns when either side closes (EOF) or an error occurs.
pub async fn run_relay(write_fd: RawFd, read_fd: RawFd) -> Result<()> {
    let _guard = TerminalGuard::enter();

    // Dup the fds so we own them independently of the VM backend
    let write_fd_dup = unsafe { libc::dup(write_fd) };
    let read_fd_dup = unsafe { libc::dup(read_fd) };
    if write_fd_dup < 0 || read_fd_dup < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    let guest_writer = unsafe { std::fs::File::from_raw_fd(write_fd_dup) };
    let guest_reader = unsafe { std::fs::File::from_raw_fd(read_fd_dup) };

    let mut guest_writer = tokio::fs::File::from_std(guest_writer);
    let mut guest_reader = tokio::fs::File::from_std(guest_reader);

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // stdin -> guest
    let stdin_to_guest = async {
        let mut buf = [0u8; 1024];
        loop {
            let n = stdin.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            guest_writer.write_all(&buf[..n]).await?;
            guest_writer.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    // guest -> stdout
    let guest_to_stdout = async {
        let mut buf = [0u8; 4096];
        loop {
            let n = guest_reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            stdout.write_all(&buf[..n]).await?;
            stdout.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    };

    // Run both directions concurrently, stop when either ends
    tokio::select! {
        r = stdin_to_guest => { r?; }
        r = guest_to_stdout => { r?; }
    }

    // _guard drops here, restoring terminal
    Ok(())
}
