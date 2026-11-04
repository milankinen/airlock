//! CLI server of a fake running sandbox, with a fake guest, and clients of
//! the server.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use airlock_common::supervisor_capnp::{listen_handle, local_connector, process, supervisor};
use airlock_monitor::NetworkEvent;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};

use super::network::network_from_toml;
use super::oci::verified_dir;
use super::sandboxes::{data_dir_sandbox, stopped_sandbox_dir};
use super::{TempDir, eventually, eventually_async, resolve_project_toml, temp_dir, test_context};
use crate::approval::MonitorSlot;
use crate::config::config_values::PortMapping;
use crate::context::Context;
use crate::network::NetworkControl;
use crate::network::io::{ChannelSink, RelayError, RpcTransport, rpc_pipe};
use crate::network::reverse_forward::relay_stream;
use crate::rpc::Supervisor;
use crate::sandbox::lock::{self, HeldLock, LockKind};
use crate::sandbox::process::client::{
    Launcher, Located, Session, StartedProcess, attach_stream, connect_or_none,
};
use crate::sandbox::process::forward::forward;
use crate::sandbox::process::init::Ready;
use crate::sandbox::process::launch::SandboxProcessChild;
use crate::sandbox::process::server::connections::{ConnectionTracker, Hold};
use crate::sandbox::process::server::monitor::MonitorFeed;
use crate::sandbox::process::server::{self as cli_server, ExecDefaults, ForwardRules};
use crate::sandbox::{ForwardMode, ForwardReport, Sandbox};
use crate::util::verified_path::VerifiedPath;
use crate::vault::{Vault, VaultStorageType};

/// Make a stopped sandbox in a temp dir, as the sandbox lookup finds it.
/// Returns:
///   The temp dir, and the sandbox. Its socket does not exist.
pub fn stopped_located() -> (TempDir, Located) {
    let tmp = temp_dir();
    let project = std::fs::canonicalize(tmp.path()).unwrap();
    let dir = project.join("sandbox");
    stopped_sandbox_dir(&dir);
    let dir = verified_dir(&dir);
    let located = Located {
        sock: dir.join("cli.sock").unwrap(),
        sandbox: Sandbox::new("test".into(), verified_dir(&project), dir),
    };
    (tmp, located)
}

/// Make a registered, stopped sandbox in the data directory of a new test
/// context, as the sandbox lookup finds it. Call it out of a runtime.
/// Returns:
///   The home of the context, the context, and the sandbox. Its socket
///   does not exist.
pub fn registered_located() -> (TempDir, Context, Located) {
    let home = temp_dir();
    let context = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let project = home.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    let (id, dir) = data_dir_sandbox(&context, &project);
    let dir = verified_dir(&dir);
    let located = Located {
        sock: crate::cache::cli_sock_path(&context.data, &dir).unwrap(),
        sandbox: Sandbox::new(id, verified_dir(&project), dir),
    };
    (home, context, located)
}

/// Take the lock `kind` of `sandbox` as a sandbox process does.
/// Returns:
///   The held lock. It stays until the value drops.
pub fn hold_lock(sandbox: &Located, kind: LockKind) -> HeldLock {
    // A fork in a parallel test can hold a copy of the file for a short
    // time.
    eventually("sandbox lock", || {
        lock::try_lock(sandbox.sandbox.dir(), kind).unwrap()
    })
}

/// Serve the CLI socket of `sandbox` as a detached sandbox process with no
/// config and no connection.
pub async fn serve_located(sandbox: &Located) -> CliSandbox {
    CliSandbox::start_at(sandbox.sock.clone(), "", false, LoopbackGuest::client()).await
}

/// Serve the CLI socket of `sandbox` as a sandbox process that a parent
/// in this process started. A stale socket file goes first, as in the
/// sandbox process.
/// Returns:
///   The server, and the session of the parent on the first connection.
pub async fn serve_located_with_parent(sandbox: &Located) -> (CliSandbox, Session) {
    let _ = sandbox.sock.remove_file();
    let (parent, child) = crate::util::fd::socket_pair().unwrap();
    let served = CliSandbox::start_with_first(
        sandbox.sock.clone(),
        "",
        false,
        LoopbackGuest::client(),
        Some(child),
    )
    .await;
    let attached = attach_stream(parent).await.unwrap();
    (served, attached)
}

/// Time limit of one connect try in the tests.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Time limit for the first connection of a sandbox that must not stop for
/// it.
pub const NO_LIMIT: Duration = Duration::from_mins(10);

/// Time in which a wait for a sandbox must not complete.
pub const STILL_WAITS: Duration = Duration::from_millis(300);

/// Check that `future` does not complete in [`STILL_WAITS`].
/// Args:
///  - `future`: The wait. It can go on after the check
///  - `what`: The panic text if the wait completes
pub async fn assert_still_waits(future: Pin<&mut impl Future>, what: &str) {
    tokio::select! {
        _ = future => panic!("{what}"),
        () = tokio::time::sleep(STILL_WAITS) => {}
    }
}

/// Error text of a sandbox process of [`FakeLauncher`] that fails.
pub const BOOT_FAILED: &str = "boot failed";

/// Launcher that runs the sandbox process in this process. A serving
/// sandbox process keeps the run lock and the server, as a real one does.
#[derive(Default)]
pub struct FakeLauncher {
    /// The sandbox process fails before it is ready.
    fail: bool,
    launches: Cell<usize>,
    processes: RefCell<Vec<(HeldLock, CliSandbox)>>,
}

impl FakeLauncher {
    /// Make a launcher whose sandbox processes fail before they are ready.
    pub fn failing() -> Self {
        Self {
            fail: true,
            ..Self::default()
        }
    }

    /// Get the number of sandbox process starts.
    pub fn launches(&self) -> usize {
        self.launches.get()
    }

    /// Get the connection count of the first started sandbox process.
    pub fn first_connections(&self) -> u32 {
        self.processes.borrow()[0].1.tracker.connections()
    }
}

impl Launcher for FakeLauncher {
    async fn launch<T>(
        &self,
        sandbox: &Located,
        run: HeldLock,
        startup: HeldLock,
        connect: impl AsyncFnOnce(&Session) -> anyhow::Result<T>,
    ) -> anyhow::Result<(Session, StartedProcess, T)> {
        self.launches.set(self.launches.get() + 1);
        if self.fail {
            anyhow::bail!(BOOT_FAILED);
        }
        // The parent has the first connection. The detach frees the
        // startup lock.
        let (process, session) = serve_located_with_parent(sandbox).await;
        let value = connect(&session).await?;
        drop(startup);
        self.processes.borrow_mut().push((run, process));
        // The exit code never comes: the fake process does not exit.
        let (_, exit) = oneshot::channel();
        let started = StartedProcess {
            ready: Ready {
                main_argv: Some(vec!["true".into()]),
            },
            child: SandboxProcessChild { pid: 0, exit },
        };
        Ok((session, started, value))
    }
}

/// Sandbox cwd of the fake sandbox.
pub const SANDBOX_CWD: &str = "/sandbox/cwd";

/// Host project dir of the fake sandbox, as the monitor feed gives it.
pub const PROJECT_PATH: &str = "/host/project";

/// Guest stats of the fake guest: per-core CPU, total and used memory,
/// load average.
pub const GUEST_STATS: (&[u8], u64, u64, (f32, f32, f32)) =
    (&[10, 90], 4096, 1024, (0.5, 1.0, 1.5));

/// Guest supervisor with only the two loopback calls, fixed stats
/// ([`GUEST_STATS`]) and a `spawn` that records the cwd and then fails. The
/// guest loopback is the host loopback.
#[derive(Default)]
pub struct LoopbackGuest {
    spawn_cwds: Rc<RefCell<Vec<String>>>,
}

impl LoopbackGuest {
    /// Make the client of the guest over a real RPC connection.
    pub fn client() -> supervisor::Client {
        Self::recording().0
    }

    /// Make the client of the guest over a real RPC connection.
    /// Returns:
    ///   The client, and the cwds of the `spawn` calls in call order.
    pub fn recording() -> (supervisor::Client, Rc<RefCell<Vec<String>>>) {
        let guest = Self::default();
        let cwds = guest.spawn_cwds.clone();
        let client = capnp_rpc::new_client::<supervisor::Client, _>(guest).client;
        (super::rpc_loopback(client), cwds)
    }
}

impl supervisor::Server for LoopbackGuest {
    async fn poll_stats(
        self: Rc<Self>,
        _params: supervisor::PollStatsParams,
        mut results: supervisor::PollStatsResults,
    ) -> Result<(), capnp::Error> {
        let (per_core, total, used, (one, five, fifteen)) = GUEST_STATS;
        let mut snap = results.get().init_snapshot();
        snap.reborrow().init_cpu().set_per_core(per_core)?;
        let mut mem = snap.reborrow().init_memory();
        mem.set_total_bytes(total);
        mem.set_used_bytes(used);
        let mut load = snap.init_load_average();
        load.set_one(one);
        load.set_five(five);
        load.set_fifteen(fifteen);
        Ok(())
    }

    async fn spawn(
        self: Rc<Self>,
        params: supervisor::SpawnParams,
        _results: supervisor::SpawnResults,
    ) -> Result<(), capnp::Error> {
        let cwd = params.get()?.get_cwd()?.to_str()?.to_string();
        self.spawn_cwds.borrow_mut().push(cwd);
        Err(capnp::Error::failed("no process in fake guest".to_string()))
    }

    async fn connect_to_guest_port(
        self: Rc<Self>,
        params: supervisor::ConnectToGuestPortParams,
        mut results: supervisor::ConnectToGuestPortResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let stream = TcpStream::connect(("127.0.0.1", params.get_port()))
            .await
            .map_err(|e| capnp::Error::failed(e.to_string()))?;
        let (host, sink) = rpc_pipe(params.get_client()?);
        tokio::task::spawn_local(relay_stream(stream, host));
        results.get().set_server(sink);
        Ok(())
    }

    async fn bind_host_port(
        self: Rc<Self>,
        params: supervisor::BindHostPortParams,
        mut results: supervisor::BindHostPortResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let port = params.get_port();
        let connector = params.get_connector()?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| capnp::Error::failed(format!("bind 127.0.0.1:{port}: {e}")))?;
        let task = tokio::task::spawn_local(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::task::spawn_local(guest_connection(stream, connector.clone()));
            }
        });
        results
            .get()
            .set_handle(capnp_rpc::new_client(GuestListener(task.abort_handle())));
        Ok(())
    }
}

/// The spawn of a process in [`ExecGuest`]: command, arguments, cwd and
/// env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    /// Command.
    pub cmd: String,
    /// Arguments of the command.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: String,
    /// Environment (`KEY=VALUE`).
    pub env: Vec<String>,
}

/// Guest supervisor with only `spawn`. Each process writes `stdout` and
/// exits with `exit`. With `exit` `None`, the process never ends.
pub struct ExecGuest {
    stdout: &'static [u8],
    exit: Option<i32>,
    spawned: Rc<RefCell<Vec<Spawned>>>,
}

impl ExecGuest {
    /// Make the client of the guest over a real RPC connection.
    /// Returns:
    ///   The client, and the spawns in call order.
    pub fn client(
        stdout: &'static [u8],
        exit: Option<i32>,
    ) -> (supervisor::Client, Rc<RefCell<Vec<Spawned>>>) {
        let spawned = Rc::default();
        let guest = Self {
            stdout,
            exit,
            spawned: Rc::clone(&spawned),
        };
        let client = capnp_rpc::new_client::<supervisor::Client, _>(guest).client;
        (super::rpc_loopback(client), spawned)
    }
}

impl supervisor::Server for ExecGuest {
    async fn spawn(
        self: Rc<Self>,
        params: supervisor::SpawnParams,
        mut results: supervisor::SpawnResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let texts = |list: capnp::text_list::Reader<'_>| -> Result<Vec<String>, capnp::Error> {
            list.iter().map(|t| Ok(t?.to_str()?.to_string())).collect()
        };
        self.spawned.borrow_mut().push(Spawned {
            cmd: params.get_cmd()?.to_str()?.to_string(),
            args: texts(params.get_args()?)?,
            cwd: params.get_cwd()?.to_str()?.to_string(),
            env: texts(params.get_env()?)?,
        });
        results.get().set_proc(capnp_rpc::new_client(FakeProcess {
            stdout: RefCell::new(Some(self.stdout)),
            exit: self.exit,
        }));
        Ok(())
    }
}

/// Process of [`ExecGuest`].
struct FakeProcess {
    stdout: RefCell<Option<&'static [u8]>>,
    exit: Option<i32>,
}

impl process::Server for FakeProcess {
    async fn poll(
        self: Rc<Self>,
        _params: process::PollParams,
        mut results: process::PollResults,
    ) -> Result<(), capnp::Error> {
        let stdout = self.stdout.borrow_mut().take();
        if let Some(data) = stdout {
            results.get().init_next().init_stdout().set_data(data);
            return Ok(());
        }
        let Some(code) = self.exit else {
            return std::future::pending().await;
        };
        results.get().init_next().set_exit(code);
        Ok(())
    }
}

/// Send one accepted guest connection to the host connector. A refused
/// connect closes the stream.
async fn guest_connection(stream: TcpStream, connector: local_connector::Client) {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: RelayError = Rc::new(RefCell::new(None));
    let mut req = connector.connect_request();
    req.get()
        .set_client(capnp_rpc::new_client(ChannelSink::new(tx, error)));
    let Ok(response) = req.send().promise.await else {
        return;
    };
    let Ok(sink) = response
        .get()
        .and_then(local_connector::connect_results::Reader::get_server)
    else {
        return;
    };
    relay_stream(stream, RpcTransport::new(Bytes::new(), rx, sink)).await;
}

/// Lifetime token of a fake guest listener. Its drop stops the listener.
struct GuestListener(AbortHandle);

impl listen_handle::Server for GuestListener {}

impl Drop for GuestListener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// CLI server of a fake running sandbox.
pub struct CliSandbox {
    temp: Option<TempDir>,
    /// Socket of the server.
    pub sock: VerifiedPath,
    /// Server task. Its abort is the sandbox shutdown.
    pub server: JoinHandle<()>,
    /// Live network policy of the sandbox.
    pub network: NetworkControl,
    /// Connection count of the sandbox.
    pub tracker: Rc<ConnectionTracker>,
    /// A hold that takes the place of the parent connection, so that the
    /// sandbox does not stop when a test client disconnects. `None` for a
    /// sandbox that counts only the test clients.
    pub keeper: Option<Hold>,
    /// Sender of the network events of the sandbox.
    pub events: broadcast::Sender<NetworkEvent>,
    /// Place of the monitor dialog of the sandbox process.
    pub approvals: MonitorSlot,
}

impl CliSandbox {
    /// Serve the CLI socket for a sandbox with the config `toml` and the
    /// fake guest `guest`. With `public_only`, the sandbox has the network
    /// of the install boot. A keeper holds the sandbox, so that it does
    /// not stop when the test clients disconnect.
    pub async fn start(toml: &str, public_only: bool, guest: supervisor::Client) -> Self {
        let dir = temp_dir();
        let sock = verified_dir(dir.path()).join("cli.sock").unwrap();
        let mut sandbox = Self::start_at(sock, toml, public_only, guest).await;
        sandbox.temp = Some(dir);
        sandbox.keeper = sandbox.tracker.open();
        sandbox
    }

    /// Serve the CLI socket `sock` (see [`Self::start`]), with no keeper.
    /// The first test client is the first connection.
    pub async fn start_at(
        sock: VerifiedPath,
        toml: &str,
        public_only: bool,
        guest: supervisor::Client,
    ) -> Self {
        Self::start_with_first(sock, toml, public_only, guest, None).await
    }

    /// Serve the CLI socket `sock` (see [`Self::start_at`]). The server
    /// also serves `first` as its first client connection, as the sandbox
    /// process does for its parent.
    pub async fn start_with_first(
        sock: VerifiedPath,
        toml: &str,
        public_only: bool,
        guest: supervisor::Client,
        first: Option<std::os::unix::net::UnixStream>,
    ) -> Self {
        let config = resolve_project_toml(toml).unwrap().values;
        let mut network = network_from_toml(toml).unwrap().network;
        if public_only {
            network = network.public_only();
        }
        let events = network.events.clone();
        let handle = network.handle();
        let network = handle.control();
        let rules = ForwardRules::new(&config.network, network.clone());
        let tracker = ConnectionTracker::new();
        let first = first.map(|stream| {
            stream.set_nonblocking(true).unwrap();
            cli_server::FirstClient {
                stream: tokio::net::UnixStream::from_std(stream).unwrap(),
                hold: tracker.open().unwrap(),
            }
        });
        let approvals = MonitorSlot::default();
        let defaults = ExecDefaults {
            env: vec![],
            cwd: SANDBOX_CWD.to_string(),
        };
        let server = tokio::task::spawn_local(cli_server::serve(
            sock.clone(),
            Supervisor::from_client(guest),
            defaults,
            rules,
            tracker.clone(),
            cli_server::no_notices(),
            MonitorFeed {
                network: handle,
                project_path: PROJECT_PATH.to_string(),
                approvals: approvals.clone(),
            },
            first,
        ));
        eventually_async("cli socket", async || {
            sock.symlink_metadata().is_ok().then_some(())
        })
        .await;
        Self {
            temp: None,
            sock,
            server,
            network,
            events,
            tracker,
            keeper: None,
            approvals,
        }
    }

    /// Run the forward command against this sandbox.
    /// Returns:
    ///   Sender that stops the command, and the command result. The result
    ///   comes after the forwards are active or after the command fails.
    pub async fn forward(
        &self,
        mode: ForwardMode,
        forwards: &[PortMapping],
    ) -> (Running, Option<anyhow::Result<()>>) {
        let session = connect_or_none(&self.sock).await.unwrap().unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (ready_tx, ready_rx) = oneshot::channel::<()>();
        let report = Rc::new(Recorder {
            ready: RefCell::new(Some(ready_tx)),
            failures: RefCell::new(Vec::new()),
        });
        let forwards = forwards.to_vec();
        let task_report = report.clone();
        let mut task = tokio::task::spawn_local(async move {
            // Only a sent value stops the command. A dropped sender does not.
            let stop = async {
                if stop_rx.await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            let Session {
                service,
                connection,
            } = session;
            forward(service, connection, mode, &forwards, stop, task_report).await
        });
        let result = tokio::select! {
            biased;
            result = &mut task => Some(result.unwrap()),
            _ = ready_rx => None,
        };
        let running = Running {
            stop: stop_tx,
            task,
            report,
        };
        (running, result)
    }
}

/// Report that records the state changes of the forwards.
struct Recorder {
    ready: RefCell<Option<oneshot::Sender<()>>>,
    failures: RefCell<Vec<(u16, String)>>,
}

impl ForwardReport for Recorder {
    fn ready(&self, _mode: ForwardMode, _forwards: &[PortMapping]) {
        let _ = self.ready.borrow_mut().take().unwrap().send(());
    }

    fn connect_failed(&self, guest_port: u16, reason: &str) {
        self.failures
            .borrow_mut()
            .push((guest_port, reason.to_string()));
    }
}

/// A forward command that runs.
pub struct Running {
    stop: oneshot::Sender<()>,
    /// Task of the command.
    pub task: JoinHandle<anyhow::Result<()>>,
    report: Rc<Recorder>,
}

impl Running {
    /// Get the ports and reasons of the reported connect failures.
    pub fn failures(&self) -> Vec<(u16, String)> {
        self.report.failures.borrow().clone()
    }

    /// Stop the command as Ctrl+C does and get its result.
    pub async fn stop(self) -> anyhow::Result<()> {
        let _ = self.stop.send(());
        self.task.await.unwrap()
    }
}

/// Client connection to the CLI server. Its drop ends the connection.
pub struct CliClient {
    /// Root cap of the server.
    pub service: cli_service::Client,
    session: Session,
}

impl CliClient {
    /// Connect to the server on `sock`.
    pub async fn connect(sock: &VerifiedPath) -> Self {
        let session = connect_or_none(sock).await.unwrap().unwrap();
        Self {
            service: session.service.clone(),
            session,
        }
    }

    /// Call `unref`.
    /// Returns:
    ///   `true` if the sandbox stops.
    pub async fn unref(&self) -> Result<bool, capnp::Error> {
        let response = self.service.unref_request().send().promise.await?;
        Ok(response.get()?.get_stopping())
    }

    /// Call `status`.
    /// Returns:
    ///   The connection count and the stop state.
    pub async fn status(&self) -> Result<(u32, bool), capnp::Error> {
        let response = self.service.status_request().send().promise.await?;
        let status = response.get()?;
        Ok((status.get_connections(), status.get_stopping()))
    }

    /// End the connection.
    pub fn disconnect(self) {
        drop(self.session);
    }
}

/// Send `text` to `127.0.0.1:<port>` and read the answer of the same
/// length.
/// Returns:
///   The answer, or `None` if the connection closes first.
pub async fn roundtrip(port: u16, text: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    stream.write_all(text.as_bytes()).await.ok()?;
    let mut buf = vec![0u8; text.len()];
    stream.read_exact(&mut buf).await.ok()?;
    Some(String::from_utf8(buf).unwrap())
}

/// Check if `127.0.0.1:<port>` is free.
pub fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Make a forward from `host` to `guest`.
pub fn mapping(host: u16, guest: u16) -> PortMapping {
    PortMapping { host, guest }
}
