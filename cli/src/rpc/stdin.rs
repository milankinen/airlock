use std::cell::RefCell;
use std::rc::Rc;

use ezpez_protocol::supervisor_capnp::*;
use tokio::io::AsyncReadExt;
use tokio::signal::unix::Signal;

pub struct Stdin {
    reader: RefCell<tokio::io::Stdin>,
    resizes: RefCell<Option<Signal>>,
    pty_size: Option<(u16, u16)>,
}

impl Stdin {
    pub fn new(
        reader: tokio::io::Stdin,
        pty_size: Option<(u16, u16)>,
        resizes: Option<Signal>,
    ) -> Self {
        Self {
            reader: RefCell::new(reader),
            resizes: RefCell::new(resizes),
            pty_size,
        }
    }

    pub fn pty_size(&self) -> Option<(u16, u16)> {
        self.pty_size
    }
}

impl stdin::Server for Stdin {
    // Single-threaded runtime; RefCell is appropriate here.
    #[allow(clippy::await_holding_refcell_ref)]
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        let mut reader = self.reader.borrow_mut();
        let mut resizes = self.resizes.borrow_mut();
        let mut buf = [0u8; 4096];

        let resize_fut = async {
            match resizes.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) | Err(_) => {
                        tracing::trace!("host stdin: EOF");
                        results.get().init_input().init_stdin().set_eof(());
                    }
                    Ok(n) => {
                        tracing::trace!("host stdin: {} bytes: {:?}", n, String::from_utf8_lossy(&buf[..n]));
                        results.get().init_input().init_stdin().set_data(&buf[..n]);
                    }
                }
            }
            () = resize_fut => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                tracing::trace!("host stdin: resize {}x{}", cols, rows);
                let mut size = results.get().init_input().init_resize();
                size.set_rows(rows);
                size.set_cols(cols);
            }
        }

        Ok(())
    }
}
