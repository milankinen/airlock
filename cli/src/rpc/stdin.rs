use ezpez_protocol::supervisor_capnp::*;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::io::AsyncReadExt;
use tokio::signal::unix::Signal;

pub struct StdinImpl {
    reader: RefCell<tokio::io::Stdin>,
    resizes: RefCell<Signal>,
}

impl StdinImpl {
    pub fn new(resizes: Signal) -> Self {
        Self {
            reader: RefCell::new(tokio::io::stdin()),
            resizes: RefCell::new(resizes),
        }
    }
}

impl stdin::Server for StdinImpl {
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        let mut reader = self.reader.borrow_mut();
        let mut resizes = self.resizes.borrow_mut();
        let mut buf = [0u8; 4096];

        tokio::select! {
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => results.get().init_input().init_stdin().set_eof(()),
                    Ok(n) => results.get().init_input().init_stdin().set_data(&buf[..n]),
                    Err(_) => results.get().init_input().init_stdin().set_eof(()),
                }
            }
            _ = resizes.recv() => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let mut size = results.get().init_input().init_resize();
                size.set_rows(rows);
                size.set_cols(cols);
            }
        }

        Ok(())
    }
}
