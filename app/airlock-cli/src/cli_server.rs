/// Unix-socket Cap'n Proto server bridging `airlock exec` clients into the running VM.
///
/// `airlock start` spawns this server after the VM is up. `airlock exec` connects here
/// and calls `CliService.exec()`. The exec's `env` list is interpreted as
/// *overrides* layered on top of the sandbox's resolved base env (image env +
/// `airlock.toml` env) — so the exec client never has to know what the sandbox
/// was launched with. The merged env is then forwarded to the supervisor
/// along with the bridged stdin/process capabilities.
use std::path::PathBuf;
use std::rc::Rc;

use airlock_common::cli_capnp::*;
use airlock_common::supervisor_capnp::*;
use futures::AsyncReadExt;

use crate::rpc::{Process, ProcessEvent, Supervisor};

/// RAII guard that removes the Unix socket file when dropped.
struct SockGuard(PathBuf);

impl Drop for SockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Accept `airlock exec` connections on a Unix socket and bridge each into the
/// running VM supervisor via Cap'n Proto RPC. `base_env` is the sandbox's
/// resolved environment (image env + config env) — `exec` clients send
/// overrides which are merged onto this before each child is spawned.
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
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let sup = supervisor.clone();
                let env = base_env.clone();
                handle_connection(stream, sup, env);
            }
            Err(e) => {
                tracing::debug!("cli server accept error: {e}");
                break;
            }
        }
    }
}

/// Set up a Cap'n Proto RPC system for a single `airlock exec` client connection.
fn handle_connection(
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
    tokio::task::spawn_local(rpc);
}

/// Implements the `CliService` Cap'n Proto interface exposed to `airlock exec` clients.
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
        let overrides: Vec<String> = params
            .get_env()?
            .iter()
            .map(|e| e.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        let env = merge_env(&self.base_env, &overrides);

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

/// Layer `KEY=VALUE` overrides over `base`. For each override, any prior
/// entry with the same key is dropped and the override is appended at the
/// end — matching the precedence `vm::resolve_env` uses for
/// `airlock.toml` over image env.
fn merge_env(base: &[String], overrides: &[String]) -> Vec<String> {
    let mut out: Vec<String> = base.to_vec();
    for entry in overrides {
        let Some((key, _)) = entry.split_once('=') else {
            continue;
        };
        let prefix = format!("{key}=");
        out.retain(|e| !e.starts_with(&prefix));
        out.push(entry.clone());
    }
    out
}

/// Bridges `Stdin.read()` calls from the vsock supervisor to the `airlock exec`
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

/// Bridges `Process.poll()`/`signal()` calls from the `airlock exec` client to
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_appends_new_keys_and_replaces_existing() {
        let base = vec!["PATH=/bin".into(), "HOME=/root".into(), "TERM=xterm".into()];
        let out = merge_env(
            &base,
            &[
                "HOME=/tmp".into(), // overrides existing
                "NEW=1".into(),     // appended
            ],
        );
        assert_eq!(
            out,
            vec![
                "PATH=/bin".to_string(),
                "TERM=xterm".into(),
                "HOME=/tmp".into(),
                "NEW=1".into(),
            ]
        );
    }

    #[test]
    fn merge_ignores_malformed_entries() {
        let base = vec!["A=1".to_string()];
        let out = merge_env(&base, &["malformed".into(), "B=2".into()]);
        assert_eq!(out, vec!["A=1".to_string(), "B=2".into()]);
    }
}
