/// Unix-socket Cap'n Proto server bridging `ez exec` clients into the running VM.
///
/// `ez go` spawns this server after the VM is up. `ez exec` connects here and
/// calls `CliService.exec()`. The server bridges the exec's `Stdin` capability
/// through to the supervisor, and wraps the returned `Process` to relay output
/// back over the unix socket.
use std::path::PathBuf;
use std::rc::Rc;

use ezpez_protocol::supervisor_capnp::*;
use futures::AsyncReadExt;

use crate::rpc::{Process, ProcessEvent, Supervisor};

/// RAII guard that removes the Unix socket file when dropped.
struct SockGuard(PathBuf);

impl Drop for SockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Accept `ez exec` connections on a Unix socket and bridge each into the
/// running VM supervisor via Cap'n Proto RPC.
pub async fn serve(sock_path: PathBuf, supervisor: Supervisor) {
    let _ = tokio::fs::remove_file(&sock_path).await;
    let listener = match tokio::net::UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cli server bind failed: {e}");
            return;
        }
    };
    let _guard = SockGuard(sock_path);

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let sup = supervisor.clone();
                handle_connection(stream, sup);
            }
            Err(e) => {
                tracing::debug!("cli server accept error: {e}");
                break;
            }
        }
    }
}

/// Set up a Cap'n Proto RPC system for a single `ez exec` client connection.
fn handle_connection(stream: tokio::net::UnixStream, supervisor: Supervisor) {
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader,
        writer,
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    let service: cli_service::Client = capnp_rpc::new_client(CliServiceImpl { supervisor });
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(service.client));
    tokio::task::spawn_local(rpc);
}

/// Implements the `CliService` Cap'n Proto interface exposed to `ez exec` clients.
struct CliServiceImpl {
    supervisor: Supervisor,
}

impl cli_service::Server for CliServiceImpl {
    async fn exec(
        self: Rc<Self>,
        params: cli_service::ExecParams,
        mut results: cli_service::ExecResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;

        let pty_size = match params.get_pty()?.which() {
            Ok(pty_config::Size(size)) => {
                let size = size?;
                Some((size.get_rows(), size.get_cols()))
            }
            _ => None,
        };

        // Bridge: unix-socket Stdin → vsock Stdin
        let unix_stdin = params.get_stdin()?;
        let vsock_stdin: stdin::Client = capnp_rpc::new_client(StdinBridge { inner: unix_stdin });

        let user_cmd = params.get_cmd()?.to_str()?.to_string();
        let user_args: Vec<String> = params
            .get_args()?
            .iter()
            .map(|a| a.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let cwd = params.get_cwd()?.to_str()?.to_string();
        let env: Vec<String> = params
            .get_env()?
            .iter()
            .map(|e| e.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        let proc = self
            .supervisor
            .exec(vsock_stdin, pty_size, &user_cmd, &user_args, &cwd, &env)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;

        // Bridge: vsock Process → unix-socket Process
        results
            .get()
            .set_proc(capnp_rpc::new_client(ProcessBridge { inner: proc }));
        Ok(())
    }
}

/// Bridges `Stdin.read()` calls from the vsock supervisor to the `ez exec`
/// client's unix-socket stdin capability.
struct StdinBridge {
    inner: stdin::Client,
}

impl stdin::Server for StdinBridge {
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        let response = self
            .inner
            .read_request()
            .send()
            .promise
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        let input = response.get()?.get_input()?;
        let dest = results.get().init_input();
        match input.which()? {
            process_input::Stdin(frame) => match frame?.which()? {
                data_frame::Data(data) => dest.init_stdin().set_data(data?),
                data_frame::Eof(()) => dest.init_stdin().set_eof(()),
            },
            process_input::Resize(size) => {
                let s = size?;
                let mut r = dest.init_resize();
                r.set_rows(s.get_rows());
                r.set_cols(s.get_cols());
            }
        }
        Ok(())
    }
}

/// Bridges `Process.poll()`/`signal()` calls from the `ez exec` client to
/// the vsock-side process running inside the VM.
struct ProcessBridge {
    inner: Process,
}

impl process::Server for ProcessBridge {
    async fn poll(
        self: Rc<Self>,
        _params: process::PollParams,
        mut results: process::PollResults,
    ) -> Result<(), capnp::Error> {
        let event = self
            .inner
            .poll()
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        let mut next = results.get().init_next();
        match event {
            ProcessEvent::Exit(code) => {
                next.set_exit(code);
            }
            ProcessEvent::Stdout(data) => {
                next.init_stdout().set_data(&data);
            }
            ProcessEvent::Stderr(data) => {
                next.init_stderr().set_data(&data);
            }
        }
        Ok(())
    }

    async fn signal(
        self: Rc<Self>,
        params: process::SignalParams,
        _results: process::SignalResults,
    ) -> Result<(), capnp::Error> {
        let signum = params.get()?.get_signum();
        self.inner
            .signal(signum)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        Ok(())
    }

    async fn kill(
        self: Rc<Self>,
        _params: process::KillParams,
        _results: process::KillResults,
    ) -> Result<(), capnp::Error> {
        let _ = self.inner.signal(9).await;
        Ok(())
    }
}
