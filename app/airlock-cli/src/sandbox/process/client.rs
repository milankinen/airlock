//! Client side of the sandbox process.
//!
//! Connects to the sandbox process of a sandbox. Each connection holds the
//! sandbox. If the sandbox does not run, starts a sandbox process. Only
//! one process at a time can start a sandbox process: the others wait for
//! it, and then connect. If the sandbox stops at the same time, a client
//! waits for the stop and then starts it again. A start for `airlock
//! start` fails if the sandbox runs.
//!
//! Also requests the stop of a sandbox, and waits until a sandbox stopped.
//! Never follows a symlink at the sandbox socket.

use std::cell::Cell;
use std::ffi::OsString;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use futures::AsyncReadExt;

use super::init::{LogOpts, Mode, Ready};
use super::launch::{LaunchRequest, SandboxProcessChild, sandbox_process_program, spawn_program};
use super::vm::machine::SHUTDOWN_TIMEOUT;
use crate::cli::LogLevel;
use crate::sandbox::lock::{self, HeldLock, LockKind};
use crate::sandbox::{Interrupted, Sandbox};
use crate::util::verified_path::VerifiedPath;
use crate::{cli, messages, rpc};

/// Time limit of one connect try on the CLI socket.
pub const TRY_TIMEOUT: Duration = Duration::from_secs(10);

/// Time limit of the wait for a sandbox stop. The sandbox process stops
/// the VM in [`SHUTDOWN_TIMEOUT`], and then releases the run lock at its
/// exit.
pub const STOP_WAIT: Duration = SHUTDOWN_TIMEOUT.saturating_add(Duration::from_secs(10));

/// A sandbox and its CLI socket.
#[derive(Debug, Clone)]
pub struct Located {
    /// The sandbox.
    pub sandbox: Sandbox,
    /// CLI socket of the sandbox.
    pub sock: VerifiedPath,
}

/// RPC connection to a sandbox. Its drop ends the connection.
pub struct Connection(tokio::task::JoinHandle<()>);

impl Connection {
    /// Run the RPC connection driver `rpc_system` in a local task.
    fn spawn(rpc_system: capnp_rpc::RpcSystem<capnp_rpc::rpc_twoparty_capnp::Side>) -> Self {
        Self(tokio::task::spawn_local(async move {
            if let Err(e) = rpc_system.await {
                tracing::debug!("sandbox connection: {e}");
            }
        }))
    }

    /// Wait until the connection ends, for example when the sandbox stops.
    pub async fn closed(&mut self) {
        let _ = (&mut self.0).await;
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A session in a sandbox: one connection that holds the sandbox. The
/// session ends when the connection drops.
pub struct Session {
    /// Client of the CLI service.
    pub service: cli_service::Client,
    /// The connection. Keep it while the session is in use.
    pub connection: Connection,
}

/// Make the client on the connected socket `stream`.
fn session_on(stream: tokio::net::UnixStream) -> Session {
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
    let network = capnp_rpc::twoparty::VatNetwork::new(
        reader,
        writer,
        capnp_rpc::rpc_twoparty_capnp::Side::Client,
        capnp::message::ReaderOptions::default(),
    );
    let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(network), None);
    let service: cli_service::Client =
        rpc_system.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);
    Session {
        service,
        connection: Connection::spawn(rpc_system),
    }
}

/// The init result of a sandbox process that this process started.
pub struct StartedProcess {
    /// The ready message of the sandbox process.
    pub ready: Ready,
    /// The detached sandbox process.
    pub child: SandboxProcessChild,
}

/// Options of a sandbox process that a client starts.
pub struct LaunchOpts {
    /// Log level of the sandbox log and the guest.
    pub log_level: LogLevel,
    /// Show the detailed boot output.
    pub verbose: bool,
    /// Do not show the boot progress.
    pub quiet: bool,
    /// What the sandbox process does in its init.
    pub mode: Mode,
}

impl LaunchOpts {
    /// Get the options for a sandbox process that `exec` or `acp` starts:
    /// the log level from `AIRLOCK_LOG_LEVEL` or `fallback`, and always
    /// quiet. The start shows no status and no progress, only questions
    /// and errors. Its warnings go to the notice log of the sandbox.
    /// Args:
    ///  - `fallback`: Log level without `AIRLOCK_LOG_LEVEL`
    ///  - `mode`: [`Mode::Exec`] or [`Mode::Acp`]
    pub fn for_client(fallback: LogLevel, mode: Mode) -> Self {
        let log_level = std::env::var("AIRLOCK_LOG_LEVEL")
            .ok()
            .and_then(|v| clap::ValueEnum::from_str(&v, true).ok())
            .unwrap_or(fallback);
        Self {
            log_level,
            verbose: false,
            quiet: true,
            mode,
        }
    }

    /// Get the log options of the sandbox process.
    fn log_opts(&self) -> LogOpts {
        LogOpts {
            log_level: self.log_level,
            verbose: self.verbose,
            quiet: self.quiet,
        }
    }
}

/// Starts the sandbox process of a sandbox.
#[allow(async_fn_in_trait)]
pub trait Launcher {
    /// Start the sandbox process of `sandbox`, wait until it is ready,
    /// attach to it and let it detach.
    /// Args:
    ///  - `sandbox`: The sandbox to start
    ///  - `run`: The held run lock. The sandbox process keeps it
    ///  - `startup`: The held startup lock. The sandbox process frees it at
    ///    the detach
    ///  - `connect`: Runs on the first session after the sandbox is ready
    ///    and before the detach
    ///
    /// Returns:
    ///   The first session, the init result and the value of `connect`.
    ///   Error [`super::launch::ExitedBeforeReady`] if the init fails, the
    ///   error of `connect`, or error if the sandbox does not start.
    async fn launch<T>(
        &self,
        sandbox: &Located,
        run: HeldLock,
        startup: HeldLock,
        connect: impl AsyncFnOnce(&Session) -> anyhow::Result<T>,
    ) -> anyhow::Result<(Session, StartedProcess, T)>;
}

/// Starts the real sandbox process: a program that does the init protocol.
pub struct ProcessLauncher {
    opts: LaunchOpts,
    program: VerifiedPath,
    args: Vec<OsString>,
}

impl ProcessLauncher {
    /// Make the launcher for the sandbox process of this airlock binary.
    /// Args:
    ///  - `opts`: Options of the sandbox process
    ///
    /// Returns:
    ///   The launcher, or error if the binary is not found.
    pub fn new(opts: LaunchOpts) -> anyhow::Result<Self> {
        let (program, args) = sandbox_process_program()?;
        Ok(Self {
            opts,
            program,
            args: args.to_vec(),
        })
    }

    /// Make a launcher that starts `program` with `args` as the sandbox
    /// process. Tests give a script.
    #[cfg(test)]
    pub fn with_program(opts: LaunchOpts, program: VerifiedPath, args: Vec<OsString>) -> Self {
        Self {
            opts,
            program,
            args,
        }
    }
}

impl Launcher for ProcessLauncher {
    async fn launch<T>(
        &self,
        sandbox: &Located,
        run: HeldLock,
        startup: HeldLock,
        connect: impl AsyncFnOnce(&Session) -> anyhow::Result<T>,
    ) -> anyhow::Result<(Session, StartedProcess, T)> {
        let request = LaunchRequest {
            sandbox_id: sandbox.sandbox.id.clone(),
            // A project that fails the path check fails in the sandbox
            // process too. Its sandbox dir is then a safe directory.
            cwd: sandbox
                .sandbox
                .project()
                .unwrap_or(&sandbox.sandbox.dir)
                .clone(),
            log: self.opts.log_opts(),
            mode: self.opts.mode.clone(),
            // The standard output of `acp` is its protocol.
            capture_output: matches!(self.opts.mode, Mode::Acp),
        };
        let attach = async move |stream| {
            let session = attach_stream(stream).await?;
            let value = connect(&session).await?;
            anyhow::Ok((session, value))
        };
        let launched =
            spawn_program(&self.program, &self.args, run, startup, &request, attach).await?;
        let (session, value) = launched.connected;
        let started = StartedProcess {
            ready: launched.ready,
            child: launched.child,
        };
        Ok((session, started, value))
    }
}

/// Get a session in `sandbox`. Starts its sandbox process if the sandbox
/// does not run. The notice "waiting for the sandbox" goes to the error
/// output, also for `acp`. It shows only while another process starts or
/// stops the sandbox.
/// Args:
///  - `sandbox`: The sandbox
///  - `launcher`: Starts the sandbox process when no sandbox runs
///  - `try_timeout`: Time limit of one connect try
///
/// Returns:
///   The session, and the init result if this call started the sandbox
///   process. Error [`Interrupted`] after a Ctrl+C,
///   [`super::launch::ExitedBeforeReady`] if the sandbox process fails, or
///   error if the sandbox does not answer in `try_timeout`. A wait for a
///   sandbox that another process starts or stops has no time limit.
// The startup lock lets only one process start a sandbox process. A
// process that waits for it then finds the sandbox that the other process
// started. A sandbox that stops closes the connections until its process
// exits. Thus the run lock wait after a failed connect waits for that exit.
pub async fn ensure_with(
    sandbox: &Located,
    launcher: &impl Launcher,
    try_timeout: Duration,
) -> anyhow::Result<(Session, Option<StartedProcess>)> {
    if let Some(session) = try_attach(&sandbox.sock, try_timeout).await? {
        return Ok((session, None));
    }
    // The notice shows only when another process starts or stops the
    // sandbox. A normal start shows nothing.
    let notice = waiting_notice();
    let dir = &sandbox.sandbox.dir;
    let startup = lock::wait_lock(dir, LockKind::Startup, &notice).await?;
    if let Some(session) = try_attach(&sandbox.sock, try_timeout).await? {
        return Ok((session, None));
    }
    let run = lock::wait_lock(dir, LockKind::Run, &notice).await?;
    let (session, started, ()) = launcher
        .launch(sandbox, run, startup, async |_| Ok(()))
        .await?;
    Ok((session, Some(started)))
}

/// Start the sandbox process of `sandbox` for `airlock start`. Fails if
/// the sandbox runs. Waits for a sandbox that another process starts or
/// stops. The wait notice goes to the error output.
/// Args:
///  - `sandbox`: The sandbox
///  - `launcher`: Starts the sandbox process
///  - `try_timeout`: Time limit of one connect try
///  - `locked`: Runs when this process holds the startup lock, before the
///    running check. Its error ends the start
///  - `connect`: Runs on the first session before the detach
///
/// Returns:
///   The first session, the init result and the value of `connect`. Error
///   [`Interrupted`] after a Ctrl+C,
///   [`super::launch::ExitedBeforeReady`] if the sandbox process fails,
///   the "already running" error if the sandbox runs, or error if the
///   sandbox does not answer in `try_timeout`.
// A sandbox that a different process starts holds the startup lock. After
// the startup lock, a sandbox that answers and does not stop runs. A
// sandbox that stops closes new connections, and its process holds the run
// lock until its exit.
pub async fn start_with<T>(
    sandbox: &Located,
    launcher: &impl Launcher,
    try_timeout: Duration,
    locked: impl AsyncFnOnce() -> anyhow::Result<()>,
    connect: impl AsyncFnOnce(&Session) -> anyhow::Result<T>,
) -> anyhow::Result<(Session, StartedProcess, T)> {
    let notice = waiting_notice();
    let dir = &sandbox.sandbox.dir;
    let startup = lock::wait_lock(dir, LockKind::Startup, &notice).await?;
    locked().await?;
    // The probe connection counts while it is open. A sandbox that runs
    // has at least one more connection, so the close does not stop it.
    if try_attach(&sandbox.sock, try_timeout).await?.is_some() {
        anyhow::bail!(messages::start::START_ALREADY_RUNNING);
    }
    let run = lock::wait_lock(dir, LockKind::Run, &notice).await?;
    launcher.launch(sandbox, run, startup, connect).await
}

/// Make the callback that tells the user one time that the command waits
/// for the sandbox.
pub fn waiting_notice() -> impl Fn() {
    let shown = Cell::new(false);
    move || {
        if !shown.replace(true) {
            cli::log!("{} {}", cli::bullet(), messages::cli::WAITING_FOR_SANDBOX);
        }
    }
}

/// Start a session in the sandbox at `sock` if it runs and does not stop.
/// Returns:
///   The session, or `None` if no sandbox serves the socket or it stops.
///   Error [`Interrupted`] after a Ctrl+C, or error if the sandbox does not
///   answer in `try_timeout`.
pub async fn try_attach(
    sock: &VerifiedPath,
    try_timeout: Duration,
) -> anyhow::Result<Option<Session>> {
    if sock.symlink_metadata().is_err() {
        return Ok(None);
    }
    let attempt = tokio::select! {
        attempt = tokio::time::timeout(try_timeout, try_open(sock)) => attempt,
        () = cli::interrupted() => return Err(Interrupted.into()),
    };
    attempt.map_err(|_| anyhow::anyhow!(messages::cli::sandbox_no_answer(try_timeout.as_secs())))?
}

/// Start a session on the socket `stream` of the first client connection
/// of a sandbox process that this process started (see [`spawn_program`]).
/// Returns:
///   The session, or error if the sandbox stopped or does not answer in
///   [`TRY_TIMEOUT`].
pub async fn attach_stream(stream: std::os::unix::net::UnixStream) -> anyhow::Result<Session> {
    stream.set_nonblocking(true)?;
    let session = session_on(tokio::net::UnixStream::from_std(stream)?);
    let status = tokio::time::timeout(TRY_TIMEOUT, status_of(&session.service))
        .await
        .map_err(|_| anyhow::anyhow!(messages::cli::sandbox_no_answer(TRY_TIMEOUT.as_secs())))??;
    match status {
        Some(status) if !status.stopping => Ok(session),
        _ => anyhow::bail!(messages::cli::SANDBOX_STOPPED),
    }
}

/// State of a sandbox process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStatus {
    /// Number of connections that hold the sandbox.
    pub connections: u32,
    /// `true` if the sandbox stops.
    pub stopping: bool,
}

/// Get the state of the sandbox process on the connection `service`.
/// Returns:
///   The state, or `None` if the sandbox process closes the connection.
///   Error for other RPC errors.
pub async fn status_of(service: &cli_service::Client) -> anyhow::Result<Option<ProcessStatus>> {
    let response = match service.status_request().send().promise.await {
        Ok(response) => response,
        Err(e) if rpc::is_closed(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let status = response.get()?;
    Ok(Some(ProcessStatus {
        connections: status.get_connections(),
        stopping: status.get_stopping(),
    }))
}

/// Ask the sandbox process that serves `sock` to stop.
/// Returns:
///   `true` if the sandbox process got the request, `false` if no sandbox
///   process listens or it closes the connection. Error for other connect
///   or RPC errors, or if the sandbox process does not answer in
///   [`TRY_TIMEOUT`].
pub async fn request_stop(sock: &VerifiedPath) -> anyhow::Result<bool> {
    let ask = async {
        let Some(session) = connect_or_none(sock).await? else {
            return anyhow::Ok(false);
        };
        match session.service.stop_request().send().promise.await {
            Ok(_) => Ok(true),
            // The sandbox process closes all connections when it stops.
            Err(e) if rpc::is_closed(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    };
    tokio::time::timeout(TRY_TIMEOUT, ask)
        .await
        .map_err(|_| anyhow::anyhow!(messages::cli::sandbox_no_answer(TRY_TIMEOUT.as_secs())))?
}

/// Wait until no process holds the run lock of `sandbox_dir`: the sandbox
/// stopped and its disk is synced.
/// Args:
///  - `sandbox_dir`: Sandbox directory with the lock file
///  - `limit`: Time limit of the wait
///
/// Returns:
///   `true` when the lock is free, `false` after the time limit.
pub async fn wait_lock_free(sandbox_dir: &VerifiedPath, limit: Duration) -> bool {
    let wait = lock::wait_free_until(sandbox_dir, LockKind::Run, tokio::time::sleep(limit));
    match wait.await {
        Ok(free) => free,
        Err(e) => {
            tracing::warn!("wait for the sandbox stop: {e:#}");
            !lock::is_held(sandbox_dir, LockKind::Run)
        }
    }
}

/// Connect to `sock`, and run the connection in a local task.
/// Returns:
///   The session, or `None` if no sandbox process listens. Error for other
///   connect errors.
pub async fn connect_or_none(sock: &VerifiedPath) -> anyhow::Result<Option<Session>> {
    match sock.connect_unix_async().await {
        Ok(stream) => Ok(Some(session_on(stream))),
        Err(e) if no_listener(&e) => Ok(None),
        Err(e) => anyhow::bail!(messages::cli::sandbox_connect_failed(sock, e)),
    }
}

/// Check if the connect error `e` means that no sandbox process listens:
/// the socket is gone, or the sandbox process closes it at the same time.
fn no_listener(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::NotFound
            | ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::NotConnected
    )
}

/// Connect to `sock` and check that the sandbox does not stop. The
/// connection holds the sandbox from its accept.
/// Returns:
///   The session, or `None` if the sandbox stops or no sandbox process
///   listens. Error for other connect or RPC errors.
// A sandbox that stops closes a new connection at once. The status call
// then fails with a closed connection.
async fn try_open(sock: &VerifiedPath) -> anyhow::Result<Option<Session>> {
    let Some(session) = connect_or_none(sock).await? else {
        return Ok(None);
    };
    match status_of(&session.service).await? {
        Some(status) if !status.stopping => Ok(Some(session)),
        _ => Ok(None),
    }
}
