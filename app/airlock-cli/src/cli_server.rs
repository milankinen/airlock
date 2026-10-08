//! Server for `airlock exec`.
//!
//! Runs in `airlock start` while the sandbox is running. It lets `airlock exec`
//! start more processes in the running sandbox.
use std::path::PathBuf;
use std::rc::Rc;

use airlock_common::cli_capnp::*;
use airlock_common::supervisor_capnp::*;
use futures::AsyncReadExt;
use tokio::task::JoinSet;

use crate::rpc::{Process, ProcessEvent, Supervisor};

/// Guard that removes the Unix socket file when dropped.
struct SockGuard(PathBuf);

impl Drop for SockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Accept `airlock exec` connections and relay them to the VM supervisor.
/// Args:
///  - `sock_path`: Path of the Unix socket to listen on
///  - `supervisor`: Supervisor of the running VM
///  - `base_env`: Resolved sandbox environment (image env and config env, with
///    surrogates for masked entries). The `env` of each exec request overrides
///    values in it, so the exec client does not need to know the sandbox env.
///
/// Returns:
///   Never, unless the bind fails. The future owns the socket file and all
///   client connections. The sandbox shutdown aborts the future. This
///   closes the connections and removes the socket file.
pub async fn serve(sock_path: PathBuf, supervisor: Supervisor, base_env: Vec<String>) {
    let _ = tokio::fs::remove_file(&sock_path).await;
    let listener = match tokio::net::UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cli server bind failed: {e}");
            return;
        }
    };
    let _guard = SockGuard(sock_path);

    let base_env = Rc::new(base_env);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let sup = supervisor.clone();
                    let env = base_env.clone();
                    connections.spawn_local(handle_connection(stream, sup, env));
                }
                Err(e) => {
                    // A failed accept() must never stop the server, for example
                    // a temporary ECONNABORTED or fd exhaustion (EMFILE/ENFILE).
                    // A stop drops `_guard` and removes the socket while the VM
                    // runs. Then all later `airlock exec` calls fail. The short
                    // sleep prevents a busy loop if the error continues.
                    tracing::warn!("cli server accept error (continuing): {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            // Remove finished connections, so that the set does not grow.
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Serve the Cap'n Proto RPC for one `airlock exec` client until it disconnects.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    supervisor: Supervisor,
    base_env: Rc<Vec<String>>,
) {
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader,
        writer,
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    let service: cli_service::Client = capnp_rpc::new_client(CliServiceImpl {
        supervisor,
        base_env,
    });
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(service.client));
    if let Err(e) = rpc.await {
        tracing::debug!("cli client connection: {e}");
    }
}

/// The `CliService` Cap'n Proto interface for `airlock exec` clients.
struct CliServiceImpl {
    supervisor: Supervisor,
    base_env: Rc<Vec<String>>,
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

        // Relay: unix-socket Stdin to vsock Stdin.
        let unix_stdin = params.get_stdin()?;
        let vsock_stdin: stdin::Client = capnp_rpc::new_client(StdinBridge { inner: unix_stdin });

        let user_cmd = params.get_cmd()?.to_str()?.to_string();
        let user_args: Vec<String> = params
            .get_args()?
            .iter()
            .map(|a| a.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let cwd = params.get_cwd()?.to_str()?.to_string();
        let overrides: Vec<String> = params
            .get_env()?
            .iter()
            .map(|e| e.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        // Ignore entries without `=`.
        let overrides = overrides.iter().filter_map(|e| e.split_once('='));
        let env = crate::util::merge_env(&self.base_env, overrides, &[]);

        let proc = self
            .supervisor
            .spawn(vsock_stdin, pty_size, &user_cmd, &user_args, &cwd, &env)
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;

        // Relay: vsock Process to unix-socket Process.
        results
            .get()
            .set_proc(capnp_rpc::new_client(ProcessBridge { inner: proc }));
        Ok(())
    }
}

/// Relays `Stdin.read()` calls from the vsock supervisor to the stdin capability
/// of the `airlock exec` client (on the Unix socket).
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

/// Relays `Process` calls (`poll`, `signal`, `kill`) from the `airlock exec`
/// client to the process in the VM (on vsock).
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
