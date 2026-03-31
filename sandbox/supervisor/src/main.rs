mod vsock;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use ezpez_protocol::supervisor_capnp::*;
use futures::AsyncReadExt;
use std::cell::RefCell;
use std::os::unix::io::FromRawFd;
use std::rc::Rc;
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt};

// -- Supervisor --

struct SupervisorImpl;

impl supervisor::Server for SupervisorImpl {
    async fn ping(
        self: Rc<Self>,
        _params: supervisor::PingParams,
        mut results: supervisor::PingResults,
    ) -> Result<(), capnp::Error> {
        results.get().set_id(0);
        Ok(())
    }

    async fn exec(
        self: Rc<Self>,
        params: supervisor::ExecParams,
        mut results: supervisor::ExecResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let stdin: byte_stream::Client = params.get_stdin()?;
        let pty_config = params.get_pty()?;

        // Open PTY if requested
        let use_pty = match pty_config.which() {
            Ok(pty_config::Size(size)) => {
                let size = size?;
                Some((size.get_rows(), size.get_cols()))
            }
            _ => None,
        };

        let (pty, child) = if let Some((rows, cols)) = use_pty {
            let (pty, pts) = pty_process::open()
                .map_err(|e| capnp::Error::failed(format!("pty open failed: {e}")))?;
            pty.resize(pty_process::Size::new(rows, cols))
                .map_err(|e| capnp::Error::failed(format!("pty resize failed: {e}")))?;
            let child = pty_process::Command::new("/bin/sh")
                .arg0("-sh")
                .env("TERM", "linux")
                .spawn(pts)
                .map_err(|e| capnp::Error::failed(format!("spawn failed: {e}")))?;
            eprintln!("supervisor: shell spawned with pty ({rows}x{cols})");
            (Some(pty), child)
        } else {
            // No PTY — TODO: spawn with pipes for separate stdout/stderr
            return Err(capnp::Error::failed("non-pty exec not yet implemented".into()));
        };

        let pty = pty.unwrap();
        let (pty_reader, pty_writer) = pty.into_split();
        let pty_writer = Rc::new(RefCell::new(Some(pty_writer)));

        // Spawn task: pull from client's stdin ByteStream → write to PTY
        let pty_writer_clone = pty_writer.clone();
        tokio::task::spawn_local(async move {
            loop {
                let response = match stdin.read_request().send().promise.await {
                    Ok(r) => r,
                    Err(_) => break,
                };
                let frame = match response.get().and_then(|r| r.get_frame()) {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame.which() {
                    Ok(data_frame::Eof(())) => break,
                    Ok(data_frame::Data(Ok(bytes))) => {
                        if let Some(w) = pty_writer_clone.borrow_mut().as_mut() {
                            if w.write_all(bytes).await.is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });

        // Return Process capability
        let proc_impl = ProcessImpl {
            pty_reader: RefCell::new(Some(pty_reader)),
            pty_writer,
            child: RefCell::new(Some(child)),
        };
        results.get().set_proc(capnp_rpc::new_client(proc_impl));

        Ok(())
    }
}

// -- Process --

struct ProcessImpl {
    pty_reader: RefCell<Option<pty_process::OwnedReadPty>>,
    pty_writer: Rc<RefCell<Option<pty_process::OwnedWritePty>>>,
    child: RefCell<Option<tokio::process::Child>>,
}

impl process::Server for ProcessImpl {
    async fn poll(
        self: Rc<Self>,
        _params: process::PollParams,
        mut results: process::PollResults,
    ) -> Result<(), capnp::Error> {
        if let Some(reader) = self.pty_reader.borrow_mut().as_mut() {
            let mut buf = [0u8; 4096];
            match reader.read(&mut buf).await {
                Ok(0) => {}
                Ok(n) => {
                    let next = results.get().init_next();
                    next.init_stdout().set_data(&buf[..n]);
                    return Ok(());
                }
                Err(_) => {}
            }
        }

        // PTY closed — get exit code
        self.pty_reader.borrow_mut().take();
        let exit_code = if let Some(mut child) = self.child.borrow_mut().take() {
            match child.wait().await {
                Ok(status) => status.code().unwrap_or(1),
                Err(_) => 1,
            }
        } else {
            0
        };

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
        // Resize via the writer half (both halves share the underlying PTY)
        if let Some(writer) = self.pty_writer.borrow().as_ref() {
            let _ = writer.resize(pty_process::Size::new(size.get_rows(), size.get_cols()));
        }
        Ok(())
    }
}

// -- main --

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!(
        "supervisor: starting on vsock port {}",
        ezpez_protocol::SUPERVISOR_PORT
    );

    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    eprintln!("supervisor: listening");

    let conn_fd = vsock::accept(listen_fd)?;
    unsafe { libc::close(listen_fd) };
    eprintln!("supervisor: host connected");

    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );

    let supervisor_client: supervisor::Client = capnp_rpc::new_client(SupervisorImpl);
    let rpc = RpcSystem::new(Box::new(network), Some(supervisor_client.client));

    let local = tokio::task::LocalSet::new();
    local.run_until(rpc).await?;

    eprintln!("supervisor: host disconnected, exiting");
    Ok(())
}
