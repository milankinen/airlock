//! Server for clients that attach to a running sandbox.
//!
//! Runs in the sandbox process while the sandbox is running. Clients:
//!  * `airlock exec` starts more processes in the running sandbox
//!  * `airlock network forward` adds port forwards between the host and
//!    the sandbox while it runs
//!  * `airlock acp` serves more host MCP servers to the sandbox while it
//!    runs
//!
//! Each connection holds the sandbox. The sandbox stops when the last
//! connection closes, or when a client requests a stop. After the stop
//! decision, the server closes new connections at once. A port forward
//! gives up its hold, so it does not keep the sandbox. The server also
//! gives the connection count, the stop state and the browser notices of
//! the sandbox. A monitor client
//! gets the network events, the guest stats and the policy changes, and can
//! change the policy.
//!
//! Guest → host forwards obey the live network policy, the same as the
//! config port forwards. The server refuses forwards that make a loop with a
//! config port forward.

pub mod connections;
pub mod monitor;

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use airlock_common::cli_capnp::*;
use airlock_common::supervisor_capnp::*;
use futures::AsyncReadExt;
use tokio::task::JoinSet;

use self::connections::{ConnectionTracker, Hold};
use self::monitor::MonitorFeed;
use crate::config::config_values::Network as NetworkConfig;
use crate::messages;
use crate::network::NetworkControl;
use crate::rpc::{Process, ProcessEvent, Supervisor};
use crate::util::verified_path::VerifiedPath;

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

/// Defaults of the processes that clients start in the sandbox.
pub struct ExecDefaults {
    /// Resolved sandbox environment (image env and config env, with
    /// surrogates for masked entries). The `env` of each exec request
    /// overrides values in it, so the exec client does not need to know the
    /// sandbox env.
    pub env: Vec<String>,
    /// Sandbox cwd. An exec request with an empty `cwd` uses it.
    pub cwd: String,
}

/// Source of the browser notices for the `notices` call. Each call takes
/// the notices that the browser collected after the previous call.
pub type NoticeSource = Rc<dyn Fn() -> Vec<String>>;

/// Make a notice source that has no notices.
pub fn no_notices() -> NoticeSource {
    Rc::new(Vec::new)
}

/// The first client connection of the server: the connection of the parent
/// of the sandbox process, which does not go through the socket file.
pub struct FirstClient {
    /// The connected stream.
    pub stream: tokio::net::UnixStream,
    /// The hold of the connection. It counts from the start of the server.
    pub hold: Hold,
}

/// Guard that removes the Unix socket file when dropped.
struct SockGuard(VerifiedPath);

impl Drop for SockGuard {
    fn drop(&mut self) {
        let _ = self.0.remove_file();
    }
}

/// Accept client connections and relay them to the VM supervisor.
/// Args:
///  - `sock_path`: Path of the Unix socket to listen on
///  - `supervisor`: Supervisor of the running VM
///  - `defaults`: Env and cwd of the exec processes
///  - `forwards`: Limits for the port forwards of the clients
///  - `tracker`: Connection count of the sandbox. The owner of the sandbox
///    waits on it for the stop
///  - `notices`: Source of the browser notices
///  - `monitor`: Sources of the monitor feed
///  - `first`: A client connection that the server serves before the
///    socket file exists
///
/// Returns:
///   Never, unless the bind fails. The future owns the socket file and all
///   client connections. The sandbox shutdown aborts the future. This
///   closes the connections and removes the socket file.
// The parts are all different. A struct for them would only move the list.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    sock_path: VerifiedPath,
    supervisor: Supervisor,
    defaults: ExecDefaults,
    forwards: ForwardRules,
    tracker: Rc<ConnectionTracker>,
    notices: NoticeSource,
    monitor: MonitorFeed,
    first: Option<FirstClient>,
) {
    let shared = Rc::new(Shared {
        supervisor,
        defaults,
        forwards,
        tracker,
        notices,
        monitor,
    });
    let mut connections = JoinSet::new();
    // The first client is served before the bind. Thus no client of the
    // socket file can be the first connection.
    if let Some(FirstClient { stream, hold }) = first {
        connections.spawn_local(handle_connection(stream, shared.clone(), hold));
    }
    let _ = sock_path.remove_file();
    let listener = match sock_path.bind_unix_async() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cli server bind failed: {e}");
            return;
        }
    };
    let _guard = SockGuard(sock_path);

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    // The count changes at the accept, with no await
                    // between the stop check and the count. A sandbox that
                    // stops closes the new connection at once.
                    if let Some(hold) = shared.tracker.open() {
                        connections.spawn_local(handle_connection(stream, shared.clone(), hold));
                    }
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
// The hold of the connection goes at the end of the connection, or at the
// `unref` call.
async fn handle_connection(stream: tokio::net::UnixStream, shared: Rc<Shared>, hold: Hold) {
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader,
        writer,
        capnp_rpc::rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    let hold = Rc::new(RefCell::new(Some(hold)));
    let service: cli_service::Client = capnp_rpc::new_client(CliServiceImpl {
        shared,
        hold: hold.clone(),
    });
    let rpc = capnp_rpc::RpcSystem::new(Box::new(network), Some(service.client));
    if let Err(e) = rpc.await {
        tracing::debug!("cli client connection: {e}");
    }
    // The service can live longer than the connection. Thus end the hold
    // here.
    hold.borrow_mut().take();
}

/// State of the server that all connections share.
struct Shared {
    supervisor: Supervisor,
    defaults: ExecDefaults,
    forwards: ForwardRules,
    tracker: Rc<ConnectionTracker>,
    notices: NoticeSource,
    monitor: MonitorFeed,
}

/// The `CliService` Cap'n Proto interface for one client connection.
struct CliServiceImpl {
    shared: Rc<Shared>,
    /// Hold of the connection on the sandbox, until `unref` or the end of
    /// the connection.
    hold: Rc<RefCell<Option<Hold>>>,
}

impl std::ops::Deref for CliServiceImpl {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.shared
    }
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
        let mut cwd = params.get_cwd()?.to_str()?.to_string();
        if cwd.is_empty() {
            cwd.clone_from(&self.defaults.cwd);
        }
        let overrides: Vec<String> = params
            .get_env()?
            .iter()
            .map(|e| e.map(|s| s.to_str().unwrap_or("").to_string()))
            .collect::<Result<Vec<_>, _>>()?;

        // Ignore entries without `=`.
        let overrides = overrides.iter().filter_map(|e| e.split_once('='));
        let env = crate::util::merge_env(&self.defaults.env, overrides, &[]);

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

    async fn add_mcp_server(
        self: Rc<Self>,
        params: cli_service::AddMcpServerParams,
        mut results: cli_service::AddMcpServerResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let session = params.get_session()?.to_str()?;
        let name = params.get_name()?.to_str()?;
        for (what, value) in [("session", session), ("name", name)] {
            if !is_route_id(value) {
                return Err(capnp::Error::failed(messages::mcp::bad_route_id(
                    what, value,
                )));
            }
        }
        // The host cap and the grant pass unwrapped. A client disconnect
        // drops the host cap and the grant on the supervisor connection,
        // as with `bindHostPort`. The guest then removes the route.
        let grant = self
            .supervisor
            .add_mcp_server(session, name, params.get_host()?)
            .await?;
        results.get().set_grant(grant);
        Ok(())
    }

    async fn unref(
        self: Rc<Self>,
        _params: cli_service::UnrefParams,
        mut results: cli_service::UnrefResults,
    ) -> Result<(), capnp::Error> {
        // The drop of the hold decides the stop at once, when it is the
        // last. A second call only gives the state.
        let hold = self.hold.borrow_mut().take();
        drop(hold);
        results.get().set_stopping(self.tracker.is_stopping());
        Ok(())
    }

    async fn stop(
        self: Rc<Self>,
        _params: cli_service::StopParams,
        _results: cli_service::StopResults,
    ) -> Result<(), capnp::Error> {
        self.tracker.request_stop();
        Ok(())
    }

    async fn status(
        self: Rc<Self>,
        _params: cli_service::StatusParams,
        mut results: cli_service::StatusResults,
    ) -> Result<(), capnp::Error> {
        let mut status = results.get();
        status.set_connections(self.tracker.connections());
        status.set_stopping(self.tracker.is_stopping());
        Ok(())
    }

    async fn notices(
        self: Rc<Self>,
        _params: cli_service::NoticesParams,
        mut results: cli_service::NoticesResults,
    ) -> Result<(), capnp::Error> {
        let notices = (self.notices)();
        let len = u32::try_from(notices.len()).map_err(|e| capnp::Error::failed(e.to_string()))?;
        let mut list = results.get().init_list(len);
        for (i, notice) in (0..len).zip(&notices) {
            list.set(i, notice);
        }
        Ok(())
    }

    async fn monitor(
        self: Rc<Self>,
        params: cli_service::MonitorParams,
        mut results: cli_service::MonitorResults,
    ) -> Result<(), capnp::Error> {
        let sink = params.get()?.get_sink()?;
        let (control, policy) = self.monitor.attach(self.supervisor.clone(), sink);
        let mut out = results.get();
        out.set_control(control);
        out.set_project_path(&self.monitor.project_path);
        out.set_policy(crate::rpc::monitor::policy_to_rpc(policy));
        Ok(())
    }
}

/// Check a session or server name for the MCP route: 1-64 characters of
/// `[A-Za-z0-9_-]`. The guest uses the values in URL paths.
fn is_route_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
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
