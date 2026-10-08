//! Keyboard input from the TUI to the sandbox.
//!
//! In monitor mode, the sandbox process does not read the real terminal. The
//! TUI sends the keystrokes and terminal size changes to the sandbox instead.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::supervisor_capnp::*;
use tokio::sync::mpsc;

/// An input event sent from the TUI event loop to the RPC stdin server.
#[derive(Debug)]
pub enum TuiInputEvent {
    /// Raw bytes (keystrokes encoded for the PTY).
    Data(Vec<u8>),
    /// Terminal resize: (rows, cols).
    Resize(u16, u16),
}

/// Cap'n Proto `Stdin` server that reads keystrokes and resize events from
/// the TUI event loop instead of from `tokio::io::stdin()`.
pub struct TuiStdin {
    rx: RefCell<mpsc::Receiver<TuiInputEvent>>,
    pty_size: Option<(u16, u16)>,
}

impl TuiStdin {
    /// Create a stdin server.
    /// Args:
    ///  - `rx`: Channel that receives input events from the TUI event loop
    ///  - `pty_size`: Initial PTY size as (rows, cols), or `None` if the
    ///    process does not use a PTY.
    pub fn new(rx: mpsc::Receiver<TuiInputEvent>, pty_size: Option<(u16, u16)>) -> Self {
        Self {
            rx: RefCell::new(rx),
            pty_size,
        }
    }

    /// Initial PTY size as (rows, cols), or `None` if there is no PTY.
    pub fn pty_size(&self) -> Option<(u16, u16)> {
        self.pty_size
    }
}

impl stdin::Server for TuiStdin {
    #[allow(clippy::await_holding_refcell_ref)]
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        let mut rx = self.rx.borrow_mut();

        match rx.recv().await {
            Some(TuiInputEvent::Data(data)) => {
                tracing::trace!("tui stdin: {} bytes", data.len());
                results.get().init_input().init_stdin().set_data(&data);
            }
            Some(TuiInputEvent::Resize(rows, cols)) => {
                tracing::debug!("tui resize: {rows}x{cols}");
                let mut size = results.get().init_input().init_resize();
                size.set_rows(rows);
                size.set_cols(cols);
            }
            None => {
                tracing::trace!("tui stdin: channel closed");
                results.get().init_input().init_stdin().set_eof(());
            }
        }

        Ok(())
    }
}
