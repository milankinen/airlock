mod vsock;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use ezpez_protocol::supervisor_capnp::{output_stream, supervisor};
use futures::AsyncReadExt;
use std::os::unix::io::FromRawFd;
use std::rc::Rc;
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt};

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

    async fn open_shell(
        self: Rc<Self>,
        params: supervisor::OpenShellParams,
        mut results: supervisor::OpenShellResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let rows = params.get_rows();
        let cols = params.get_cols();
        let stdout: output_stream::Client = params.get_stdout()?;

        let (pty, pts) = pty_process::open()
            .map_err(|e| capnp::Error::failed(format!("pty open failed: {e}")))?;
        pty.resize(pty_process::Size::new(rows, cols))
            .map_err(|e| capnp::Error::failed(format!("pty resize failed: {e}")))?;

        let mut child = pty_process::Command::new("/bin/sh")
            .arg0("-sh")
            .env("TERM", "linux")
            .spawn(pts)
            .map_err(|e| capnp::Error::failed(format!("shell spawn failed: {e}")))?;

        eprintln!("supervisor: shell spawned");

        let (mut pty_reader, pty_writer) = pty.into_split();
        let pty_writer = Rc::new(std::cell::RefCell::new(pty_writer));

        // Task: read PTY output → push to client, then await child exit
        tokio::task::spawn_local(async move {
            // Relay PTY output
            let mut buf = [0u8; 4096];
            loop {
                match pty_reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut req = stdout.write_request();
                        req.get().set_data(&buf[..n]);
                        if req.send().await.is_err() {
                            break;
                        }
                    }
                }
            }

            // Wait for child to exit and get exit code
            let exit_code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(1),
                Err(_) => 1,
            };

            // Signal done with exit code
            let mut req = stdout.done_request();
            req.get().set_exit_code(exit_code);
            let _ = req.send().promise.await;
        });

        // Return stdin capability
        let stdin_impl = ShellStdinImpl { pty_writer };
        results.get().set_stdin(capnp_rpc::new_client(stdin_impl));

        Ok(())
    }
}

struct ShellStdinImpl {
    pty_writer: Rc<std::cell::RefCell<pty_process::OwnedWritePty>>,
}

impl output_stream::Server for ShellStdinImpl {
    async fn write(
        self: Rc<Self>,
        params: output_stream::WriteParams,
    ) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        let mut writer = self.pty_writer.borrow_mut();
        writer
            .write_all(data)
            .await
            .map_err(|e| capnp::Error::failed(format!("pty write failed: {e}")))?;
        Ok(())
    }

    async fn done(
        self: Rc<Self>,
        _params: output_stream::DoneParams,
        _results: output_stream::DoneResults,
    ) -> Result<(), capnp::Error> {
        Ok(())
    }
}

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
