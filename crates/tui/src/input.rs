//! Channel-based stdin replacement for TUI mode.
//!
//! Instead of reading from `tokio::io::stdin()`, the TUI event loop sends
//! keystrokes and resize events through an mpsc channel. This struct
//! implements the Cap'n Proto `Stdin` RPC interface so it can be passed
//! to the supervisor in place of the real terminal stdin.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_protocol::supervisor_capnp::*;
use tokio::sync::mpsc;

/// An input event sent from the TUI event loop to the RPC stdin server.
#[derive(Debug)]
pub enum TuiInputEvent {
    /// Raw bytes (keystrokes encoded for the PTY).
    Data(Vec<u8>),
    /// Terminal resize: (rows, cols).
    Resize(u16, u16),
}

/// Implements the Cap'n Proto `Stdin` interface by reading from a channel
/// fed by the TUI event loop.
pub struct TuiStdin {
    rx: RefCell<mpsc::Receiver<TuiInputEvent>>,
    pty_size: Option<(u16, u16)>,
}

impl TuiStdin {
    pub fn new(rx: mpsc::Receiver<TuiInputEvent>, pty_size: Option<(u16, u16)>) -> Self {
        Self {
            rx: RefCell::new(rx),
            pty_size,
        }
    }

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
