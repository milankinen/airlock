//! Server for clients that attach to a running sandbox.
//!
//! Runs in `airlock start` while the sandbox is running. Clients:
//!  * `airlock exec` starts more processes in the running sandbox
//!  * `airlock network forward` adds port forwards between the host and
//!    the sandbox while it runs
//!
//! Guest → host forwards obey the live network policy, the same as the
//! config port forwards. The server refuses forwards that make a loop with a
//! config port forward.
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use airlock_common::cli_capnp::*;
use airlock_common::supervisor_capnp::*;
use futures::AsyncReadExt;
use tokio::task::JoinSet;

use crate::config::config_values::Network as NetworkConfig;
use crate::messages;
use crate::network::NetworkControl;
use crate::rpc::{Process, ProcessEvent, Supervisor};

/// Limits for the port forwards that clients add at run time.
#[derive(Clone)]
pub struct ForwardRules {
    /// Live network policy. Guest → host forwards obey it.
    pub network: NetworkControl,
    /// Guest ports that config forwards to the host (`host` lists). The
    /// guest listens on them.
    pub to_host_guest_ports: HashSet<u16>,
    /// Guest ports that config forwards from the host (`guest` lists).
    pub from_host_guest_ports: HashSet<u16>,
}

impl ForwardRules {
    /// Make the rules from the network config and the live network policy.
    pub fn new(config: &NetworkConfig, network: NetworkControl) -> Self {
        let rules = crate::network::rules::port_forwards_from_config(config);
        let reverse = crate::network::rules::reverse_port_forwards_from_config(config);
        Self {
            network,
            to_host_guest_ports: rules.iter().map(|(guest, _)| *guest).collect(),
            from_host_guest_ports: reverse.iter().map(|(_, guest)| *guest).collect(),
        }
    }
}

/// Guard that removes the Unix socket file when dropped.
struct SockGuard(PathBuf);

impl Drop for SockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Accept client connections and relay them to the VM supervisor.
/// Args:
///  - `sock_path`: Path of the Unix socket to listen on
///  - `supervisor`: Supervisor of the running VM
///  - `base_env`: Resolved sandbox environment (image env and config env, with
///    surrogates for masked entries). The `env` of each exec request overrides
///    values in it, so the exec client does not need to know the sandbox env.
///  - `forwards`: Limits for the port forwards of the clients
///
/// Returns:
///   Never, unless the bind fails. The future owns the socket file and all
///   client connections. The sandbox shutdown aborts the future. This
///   closes the connections and removes the socket file.
pub async fn serve(
    sock_path: PathBuf,
    supervisor: Supervisor,
    base_env: Vec<String>,
    forwards: ForwardRules,
) {
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
    let forwards = Rc::new(forwards);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let sup = supervisor.clone();
                    let env = base_env.clone();
                    let fwd = forwards.clone();
                    connections.spawn_local(handle_connection(stream, sup, env, fwd));
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

/// Serve the Cap'n Proto RPC for one client until it disconnects.
// The capabilities that the client gives (sinks, connectors) go to the
// supervisor through this connection. When the client disconnects, the RPC
// system releases them. The guest then closes the listeners and connections
// of the client.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    supervisor: Supervisor,
    base_env: Rc<Vec<String>>,
    forwards: Rc<ForwardRules>,
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
        forwards,
    });
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(service.client));
    if let Err(e) = rpc.await {
        tracing::debug!("cli client connection: {e}");
    }
}

/// The `CliService` Cap'n Proto interface for the clients.
struct CliServiceImpl {
    supervisor: Supervisor,
    base_env: Rc<Vec<String>>,
    forwards: Rc<ForwardRules>,
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

    async fn connect_to_guest_port(
        self: Rc<Self>,
        params: cli_service::ConnectToGuestPortParams,
        mut results: cli_service::ConnectToGuestPortResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let port = params.get_port();
        // The guest listener of a config `host` forward sends the connection
        // back to the host.
        if self.forwards.to_host_guest_ports.contains(&port) {
            return Err(capnp::Error::failed(
                messages::network_forward::loop_to_host(port),
            ));
        }
        let server = self
            .supervisor
            .connect_to_guest_port(port, params.get_client()?)
            .await?;
        results.get().set_server(server);
        Ok(())
    }

    async fn bind_host_port(
        self: Rc<Self>,
        params: cli_service::BindHostPortParams,
        mut results: cli_service::BindHostPortResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let port = params.get_port();
        // A config `guest` forward sends host connections to this guest
        // port. A listener here sends them back to the host.
        if self.forwards.from_host_guest_ports.contains(&port) {
            return Err(capnp::Error::failed(
                messages::network_forward::loop_from_host(port),
            ));
        }
        let connector: local_connector::Client = capnp_rpc::new_client(PolicyConnector {
            inner: params.get_connector()?,
            network: self.forwards.network.clone(),
        });
        let handle = self.supervisor.bind_host_port(port, connector).await?;
        results.get().set_handle(handle);
        Ok(())
    }
}

/// Relays guest connections to the connector of the client, if the network
/// policy allows guest → host forwards at the time of the connection.
struct PolicyConnector {
    inner: local_connector::Client,
    network: NetworkControl,
}

impl local_connector::Server for PolicyConnector {
    async fn connect(
        self: Rc<Self>,
        params: local_connector::ConnectParams,
        mut results: local_connector::ConnectResults,
    ) -> Result<(), capnp::Error> {
        // The policy can change while the sandbox runs. Thus check it for
        // each connection, not at listen time.
        if !self.network.allows_port_forward() {
            return Err(capnp::Error::failed(
                messages::network_forward::DENIED_BY_POLICY.to_string(),
            ));
        }
        let mut req = self.inner.connect_request();
        req.get().set_client(params.get()?.get_client()?);
        let response = req.send().promise.await?;
        results.get().set_server(response.get()?.get_server()?);
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
