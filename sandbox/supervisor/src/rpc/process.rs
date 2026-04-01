use ezpez_protocol::supervisor_capnp::*;
use ezpez_protocol::streams::InputStream;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::io::AsyncReadExt;

pub fn spawn(
    stdin: byte_stream::Client,
    pty_size: Option<(u16, u16)>,
) -> Result<process::Client, capnp::Error> {
    let (rows, cols) = pty_size
        .ok_or_else(|| capnp::Error::failed("non-pty exec not yet implemented".into()))?;

    let (pty, pts) = pty_process::open()
        .map_err(|e| capnp::Error::failed(format!("pty open failed: {e}")))?;
    pty.resize(pty_process::Size::new(rows, cols))
        .map_err(|e| capnp::Error::failed(format!("pty resize failed: {e}")))?;

    let child = pty_process::Command::new("crun")
        .args(["run", "--no-pivot", "--bundle", "/mnt/bundle", "ezpez0"])
        .env("TERM", "linux")
        .spawn(pts)
        .map_err(|e| capnp::Error::failed(format!("spawn failed: {e}")))?;

    eprintln!("supervisor: process spawned with pty ({rows}x{cols})");

    let (pty_reader, pty_writer) = pty.into_split();
    let pty_writer = Rc::new(RefCell::new(Some(pty_writer)));

    // Stdin relay: pull from client's ByteStream → write to PTY
    let pty_writer_clone = pty_writer.clone();
    tokio::task::spawn_local(async move {
        let mut reader = InputStream::from(stdin);
        if let Some(writer) = pty_writer_clone.borrow_mut().as_mut() {
            if let Err(e) = tokio::io::copy(&mut reader, writer).await {
                eprintln!("supervisor: stdin relay error: {e}");
            }
        }
        pty_writer_clone.borrow_mut().take();
    });

    let proc_impl = ProcessImpl {
        pty_reader: RefCell::new(Some(pty_reader)),
        pty_writer,
        child: RefCell::new(Some(child)),
        cached_exit: RefCell::new(None),
    };

    Ok(capnp_rpc::new_client(proc_impl))
}

struct ProcessImpl {
    pty_reader: RefCell<Option<pty_process::OwnedReadPty>>,
    pty_writer: Rc<RefCell<Option<pty_process::OwnedWritePty>>>,
    child: RefCell<Option<tokio::process::Child>>,
    cached_exit: RefCell<Option<i32>>,
}

impl process::Server for ProcessImpl {
    async fn poll(
        self: Rc<Self>,
        _params: process::PollParams,
        mut results: process::PollResults,
    ) -> Result<(), capnp::Error> {
        if let Some(code) = *self.cached_exit.borrow() {
            results.get().init_next().set_exit(code);
            return Ok(());
        }

        if let Some(reader) = self.pty_reader.borrow_mut().as_mut() {
            let mut buf = [0u8; 4096];
            match reader.read(&mut buf).await {
                Ok(0) => {}
                Ok(n) => {
                    results.get().init_next().init_stdout().set_data(&buf[..n]);
                    return Ok(());
                }
                Err(e) => eprintln!("supervisor: pty read error: {e}"),
            }
        }

        self.pty_reader.borrow_mut().take();
        let exit_code = if let Some(mut child) = self.child.borrow_mut().take() {
            match child.wait().await {
                Ok(status) => status.code().unwrap_or(1),
                Err(e) => {
                    eprintln!("supervisor: child wait error: {e}");
                    1
                }
            }
        } else {
            self.cached_exit.borrow().unwrap_or(1)
        };

        *self.cached_exit.borrow_mut() = Some(exit_code);
        results.get().init_next().set_exit(exit_code);
        Ok(())
    }

    async fn signal(
        self: Rc<Self>,
        params: process::SignalParams,
        _results: process::SignalResults,
    ) -> Result<(), capnp::Error> {
        let signum = params.get()?.get_signum();
        if let Some(child) = self.child.borrow().as_ref() {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(pid as i32, signum as i32) };
            }
        }
        Ok(())
    }

    async fn kill(
        self: Rc<Self>,
        _params: process::KillParams,
        _results: process::KillResults,
    ) -> Result<(), capnp::Error> {
        if let Some(mut child) = self.child.borrow_mut().take() {
            let _ = child.kill().await;
        }
        Ok(())
    }

    async fn resize(
        self: Rc<Self>,
        params: process::ResizeParams,
        _results: process::ResizeResults,
    ) -> Result<(), capnp::Error> {
        let size = params.get()?.get_size()?;
        if let Some(writer) = self.pty_writer.borrow().as_ref() {
            let _ = writer.resize(pty_process::Size::new(size.get_rows(), size.get_cols()));
        }
        Ok(())
    }
}
