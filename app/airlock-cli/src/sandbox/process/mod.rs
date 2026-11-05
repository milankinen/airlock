//! Sandbox process.
//!
//! The sandbox process owns the VM of one sandbox. A client finds the
//! sandbox, then gets its sandbox process: it connects to a sandbox that
//! runs, or it takes the sandbox locks and starts the sandbox process. The
//! client then uses the sandbox through its connection: it runs
//! processes in the guest, shows the monitor, adds port forwards and MCP
//! servers, and releases or stops the sandbox. Each connection holds the
//! sandbox until it closes or the client releases it.
//!
//! Contains also the sandbox process itself: the init protocol with its
//! parent, the server for the clients, and the VM runtime.

submodules!(attach, child, client, forward, launch, server);
submodules!(pub(super) init, vm);

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use airlock_common::supervisor_capnp::{mcp_grant, mcp_host, stdin};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use self::child::run as run_sandbox_process;
use self::client::{Located, Session, StartedProcess};
pub use self::forward::{ForwardMode, ForwardReport};
use self::init::{Mode, StartArgs};
pub use self::launch::ExitedBeforeReady;
pub use self::server::notices::NoticeLog;
use super::lock::{self, LockKind};
use super::start::location::check_registered;
use super::{Interrupted, Sandbox};
use crate::approval::MonitorSlot;
use crate::cli::LogLevel;
use crate::config::config_values::PortMapping;
use crate::context::Context;
use crate::runtime::{self, MonitorSource, Runtime};
use crate::{cache, cli, messages, rpc};

/// Time limit for the exit code of the sandbox process after it released
/// the run lock.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// What a client does with a sandbox (see [`Sandbox::run`]).
pub enum Run {
    /// `airlock exec`: connect, or start with the config of the last
    /// `airlock start`. The start does not apply changes of the project
    /// config. If the project config is different from the config of the
    /// last start, the start adds a warning to the notice log of the
    /// sandbox. `airlock exec` prints all notices of the log after the
    /// connect, also when the sandbox already ran. The start shows no
    /// progress, only the vault question and errors.
    Exec,
    /// `airlock acp`: as [`Run::Exec`], but with no questions. The output
    /// of a sandbox process that fails before it is ready goes into the
    /// error.
    Acp {
        /// Log level of a sandbox process that this run starts.
        log_level: LogLevel,
    },
    /// `airlock start`: start the sandbox. Fails if the sandbox runs.
    Start {
        /// The start arguments for the sandbox process.
        args: StartArgs,
        /// Log level of the sandbox log and the guest.
        log_level: LogLevel,
        /// Show the detailed boot output.
        verbose: bool,
        /// Approval dialog of the TUI monitor, if the terminal has it.
        monitor: Option<MonitorSlot>,
    },
}

impl Run {
    /// Get the options of the sandbox process that this run starts.
    fn launch_opts(&self) -> client::LaunchOpts {
        match self {
            Self::Exec => client::LaunchOpts::for_client(LogLevel::Info, Mode::Exec),
            Self::Acp { log_level } => client::LaunchOpts::for_client(*log_level, Mode::Acp),
            Self::Start {
                args,
                log_level,
                verbose,
                ..
            } => client::LaunchOpts {
                log_level: *log_level,
                verbose: *verbose,
                quiet: cli::is_silent(),
                mode: Mode::Start { args: args.clone() },
            },
        }
    }
}

/// Result of [`Sandbox::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopResult {
    /// The sandbox did not run.
    NotRunning,
    /// The sandbox stopped and its disk is synced.
    Stopped,
    /// The sandbox did not stop in time.
    Timeout,
}

/// The time limit of [`StopResult::Timeout`], in seconds.
pub const STOP_WAIT_SECS: u64 = client::STOP_WAIT.as_secs();

impl Sandbox {
    /// Get the sandbox process of the sandbox for `run`. Connects to the
    /// sandbox if it runs. Else takes the sandbox locks and starts the
    /// sandbox process. Waits for a sandbox that another process starts or
    /// stops.
    /// Args:
    ///  - `context`: Process context with the registry and the data dir
    ///  - `run`: What the client does
    ///
    /// Returns:
    ///   The sandbox process with a held connection. Error [`Interrupted`]
    ///   after a Ctrl+C, [`ExitedBeforeReady`] if the sandbox process
    ///   fails before it is ready, the "never started" error for exec and
    ///   acp of a sandbox with no config of a last start, the "already
    ///   running" error for a start of a sandbox that runs, or error if the
    ///   sandbox does not answer.
    pub async fn run(self, context: &Context, run: Run) -> anyhow::Result<SandboxProcess> {
        let launcher = client::ProcessLauncher::new(run.launch_opts())?;
        self.run_with(context, run, &launcher, client::TRY_TIMEOUT)
            .await
    }

    /// Get the sandbox process as [`Self::run`] does, with `launcher` for
    /// the start and `try_timeout` for each connect try.
    pub(crate) async fn run_with(
        self,
        context: &Context,
        run: Run,
        launcher: &impl client::Launcher,
        try_timeout: Duration,
    ) -> anyhow::Result<SandboxProcess> {
        let located = self.locate(context)?;
        let (session, started, monitor) = match run {
            Run::Exec | Run::Acp { .. } => {
                if let Some(session) = client::try_attach(&located.sock, try_timeout).await? {
                    (session, None, None)
                } else {
                    anyhow::ensure!(
                        self.is_startable(),
                        messages::cli::sandbox_never_started(self.path().display())
                    );
                    let (session, started) =
                        client::ensure_with(&located, launcher, try_timeout).await?;
                    (session, started, None)
                }
            }
            Run::Start { monitor, .. } => {
                // The approval requests of the sandbox process show in the
                // dialog of the TUI. The monitor feed attaches before the
                // detach, so that no request comes before it. The TUI starts
                // later with the main command: a request waits for its
                // dialog until the time limit.
                let subscribe = async |session: &Session| match monitor {
                    Some(slot) => Ok(Some(runtime::subscribe(&session.service, slot).await?)),
                    None => Ok(None),
                };
                let (session, started, monitor) = client::start_with(
                    &located,
                    launcher,
                    try_timeout,
                    async || check_registered(context, &self).await,
                    subscribe,
                )
                .await?;
                info!("sandbox process ready, pid = {}", started.child.pid);
                (session, Some(started), monitor)
            }
        };
        Ok(SandboxProcess {
            sandbox: self,
            session,
            started,
            monitor,
        })
    }

    /// Connect to the sandbox process of the sandbox. Never starts the
    /// sandbox.
    /// Returns:
    ///   The sandbox process with a held connection, or error if the
    ///   sandbox does not run or does not answer.
    pub async fn connect(&self, context: &Context) -> anyhow::Result<SandboxProcess> {
        let located = self.locate(context)?;
        let session = client::try_attach(&located.sock, client::TRY_TIMEOUT)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(messages::cli::no_running_sandbox(self.path().display()))
            })?;
        Ok(SandboxProcess {
            sandbox: self.clone(),
            session,
            started: None,
            monitor: None,
        })
    }

    /// Stop the sandbox, also when clients use it, and wait until it
    /// stopped and its disk is synced. A sandbox that boots stops after its
    /// boot.
    /// Returns:
    ///   The result. Error [`Interrupted`] after a Ctrl+C, or error if the
    ///   stop request fails.
    // A sandbox process that serves the CLI socket gets the stop request. A
    // sandbox process in its init holds the startup lock, and serves the
    // socket after it. With the startup lock held, no sandbox process can
    // start or detach. A held run lock then is a sandbox process that
    // stops.
    pub async fn stop(&self, context: &Context) -> anyhow::Result<StopResult> {
        let located = self.locate(context)?;
        if client::request_stop(&located.sock).await? {
            return self.wait_lock_free().await;
        }
        let notice = client::waiting_notice();
        let startup = match lock::wait_lock(&self.dir, LockKind::Startup, notice).await {
            Ok(startup) => startup,
            // A sandbox with no directory does not run.
            Err(e) if is_not_found(&e) => return Ok(StopResult::NotRunning),
            Err(e) => return Err(e),
        };
        if client::request_stop(&located.sock).await? {
            drop(startup);
            return self.wait_lock_free().await;
        }
        let running = lock::is_held(&self.dir, LockKind::Run);
        drop(startup);
        if !running {
            return Ok(StopResult::NotRunning);
        }
        self.wait_lock_free().await
    }

    /// Wait until the sandbox process released the run lock.
    /// Returns:
    ///   [`StopResult::Stopped`], or [`StopResult::Timeout`] after
    ///   [`client::STOP_WAIT`]. Error [`Interrupted`] after a Ctrl+C.
    async fn wait_lock_free(&self) -> anyhow::Result<StopResult> {
        let stopped = tokio::select! {
            stopped = client::wait_lock_free(&self.dir, client::STOP_WAIT) => stopped,
            () = cli::interrupted() => return Err(Interrupted.into()),
        };
        Ok(if stopped {
            StopResult::Stopped
        } else {
            StopResult::Timeout
        })
    }

    /// Get the CLI socket of the sandbox.
    /// Returns:
    ///   The sandbox and its socket, or error if the sandbox directory or
    ///   the socket is a symlink.
    // A symlink at the sandbox directory or at `cli.sock` could send the
    // command, the env values and the terminal to a different sandbox.
    // Thus never follow a symlink. The socket path is `cli.sock` in the
    // sandbox directory. If that path is longer than the `AF_UNIX` limit,
    // the path is a hash-keyed fallback under `<data>/sock/`.
    fn locate(&self, context: &Context) -> anyhow::Result<Located> {
        if self.dir.symlink_metadata().is_ok() {
            self.dir.to_path()?;
        }
        let sock = cache::cli_sock_path(&context.data, &self.dir)?;
        if sock.symlink_metadata().is_ok() {
            sock.to_path()?;
        }
        Ok(Located {
            sandbox: self.clone(),
            sock,
        })
    }
}

/// Check if `error` comes from a file or a directory that does not exist.
fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    })
}

/// A process to run in the sandbox.
pub struct ExecRequest<'a> {
    /// Command and its arguments.
    pub argv: &'a [String],
    /// Working directory in the guest. An empty value is the sandbox cwd.
    pub cwd: &'a str,
    /// Environment overrides (`KEY=VALUE`).
    pub env: &'a [String],
}

/// The sandbox process of a sandbox, with a held connection. The drop
/// closes the connection.
pub struct SandboxProcess {
    /// The sandbox.
    sandbox: Sandbox,
    /// The connection.
    session: Session,
    /// The init result, if this process started the sandbox process.
    started: Option<StartedProcess>,
    /// The monitor feed of the TUI, if the start attached it.
    monitor: Option<MonitorSource>,
}

impl SandboxProcess {
    /// Get the guest dir of the host dir `host` (see
    /// [`Sandbox::guest_dir`]). Call it after the connect: a start of the
    /// sandbox can change the guest cwd.
    pub fn guest_dir(&self, host: &crate::util::VerifiedPath) -> anyhow::Result<String> {
        self.sandbox.guest_dir(host)
    }

    /// Get the project dir in the guest (see [`Sandbox::guest_root`]).
    /// Call it after the connect, as for [`Self::guest_dir`].
    pub fn guest_root(&self) -> anyhow::Result<String> {
        self.sandbox.guest_root()
    }

    /// Run `request` in the sandbox and connect it to the terminal of
    /// `runtime` until the process exits.
    /// Returns:
    ///   The exit code from the terminal. Exit code 1 if the sandbox stops
    ///   or the connection fails before the process exits. Error if the
    ///   process does not start.
    pub async fn exec(
        &mut self,
        runtime: impl Runtime,
        request: &ExecRequest<'_>,
    ) -> anyhow::Result<i32> {
        let request = attach::AttachRequest {
            argv: request.argv,
            cwd: request.cwd,
            env: request.env,
            dump_dir: None,
        };
        attach::run_attached(&self.session.service, runtime, None, &request).await
    }

    /// Run the main command of `airlock start` in the sandbox and connect
    /// it to the terminal of `runtime`, or of the TUI monitor of the start.
    /// Returns:
    ///   The exit code from the terminal, or error if this process did not
    ///   start the sandbox, or the process or the monitor does not start.
    pub async fn exec_main(&mut self, runtime: impl Runtime) -> anyhow::Result<i32> {
        // The sandbox process knows the image and gives the main command.
        let argv = self
            .started
            .as_ref()
            .and_then(|started| started.ready.main_argv.clone())
            .ok_or_else(|| anyhow::anyhow!(messages::sandbox_process::NO_MAIN_COMMAND))?;
        // An empty cwd is the sandbox cwd of the sandbox process: the guest
        // cwd of this start.
        let request = attach::AttachRequest {
            argv: &argv,
            cwd: "",
            env: &[],
            dump_dir: Some(&self.sandbox.dir),
        };
        let monitor = self.monitor.take();
        let code = attach::run_attached(&self.session.service, runtime, monitor, &request).await?;
        info!("main process exited, final code = {code}");
        Ok(code)
    }

    /// Start `request` in the sandbox with its input from `input`. The
    /// input closes when all senders drop.
    /// Returns:
    ///   The started process, or error if it does not start.
    pub async fn exec_piped(
        &self,
        request: &ExecRequest<'_>,
        input: mpsc::Receiver<Vec<u8>>,
    ) -> anyhow::Result<rpc::Process> {
        let mut req = self.session.service.exec_request();
        rpc::fill_exec_request(
            req.get(),
            capnp_rpc::new_client(ChannelStdin(RefCell::new(input))),
            None,
            request.argv,
            request.cwd,
            request.env,
        );
        let response = req.send().promise.await?;
        Ok(rpc::Process::new(response.get()?.get_proc()?))
    }

    /// Get the registrar that adds MCP servers of the host to the sandbox
    /// over this connection. Its calls fail after the connection closed.
    pub fn mcp_registrar(&self) -> McpRegistrar {
        McpRegistrar(self.session.service.clone())
    }

    /// Get all notices of the sandbox since its boot, oldest first.
    /// Returns:
    ///   The notices. A failure gives no notices and goes only to the log.
    pub async fn notices(&self) -> Vec<Notice> {
        self.notice_reader().read().await
    }

    /// Print the notices of the sandbox with a number above `after` as
    /// warnings on the error output, oldest first.
    /// Returns:
    ///   The highest number of the printed notices, or `after` if no notice
    ///   is new. Give it as `after` to the next call.
    pub async fn show_notices(&self, after: u64) -> u64 {
        let mut last = after;
        for notice in self.notices().await {
            if notice.seqno > after {
                cli::log!("{} {}", cli::yellow("!"), notice.text);
                last = last.max(notice.seqno);
            }
        }
        last
    }

    /// Get the reader of the sandbox notices over this connection. Its
    /// reads give no notices after the connection closed.
    pub fn notice_reader(&self) -> NoticeReader {
        NoticeReader(self.session.service.clone())
    }

    /// Give up the hold of this connection on the sandbox. The connection
    /// stays open.
    /// Returns:
    ///   `true` if the sandbox stops now, because no other connection holds
    ///   it, or because it stopped before.
    pub async fn release(&self) -> bool {
        match self.session.service.unref_request().send().promise.await {
            Ok(response) => response
                .get()
                .is_ok_and(cli_service::unref_results::Reader::get_stopping),
            // The sandbox stopped before, for example after `airlock stop`.
            Err(e) => {
                info!("sandbox unref: {e:#}");
                true
            }
        }
    }

    /// Run the port forwards `forwards` until `stop` completes or the
    /// sandbox stops. The forwards do not hold the sandbox.
    /// Args:
    ///  - `mode`: Direction of the forwards
    ///  - `forwards`: Port pairs. No two can use the same listening port
    ///  - `stop`: Future that completes when the user stops the command
    ///  - `report`: Receives the state changes of the forwards
    ///
    /// Returns:
    ///   `Ok` when `stop` completes. Error if a forward cannot start (then
    ///   no forward stays active), or if the sandbox stops.
    pub async fn forward(
        self,
        mode: ForwardMode,
        forwards: &[PortMapping],
        stop: impl Future<Output = ()>,
        report: Rc<dyn ForwardReport>,
    ) -> anyhow::Result<()> {
        let Session {
            service,
            connection,
        } = self.session;
        forward::forward(service, connection, mode, forwards, stop, report).await
    }

    /// Close the connection and wait until the sandbox process stopped the
    /// sandbox and released the run lock. Thus the disk is synced at the
    /// return, and the next start finds the sandbox free. A new Ctrl+C or
    /// SIGTERM ends the wait. The sandbox process stops the sandbox also
    /// then. A timeout and a failed VM stop show on the error output.
    pub async fn wait_stopped(self) {
        let Self {
            sandbox, started, ..
        } = self;
        // The interrupt flag can be set from a signal of the session. Only
        // a new signal ends the wait.
        let sigterm = async {
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    sigterm.recv().await;
                }
                Err(_) => std::future::pending().await,
            }
        };
        let stopped = tokio::select! {
            stopped = client::wait_lock_free(&sandbox.dir, client::STOP_WAIT) => stopped,
            _ = tokio::signal::ctrl_c() => return,
            () = sigterm => return,
        };
        if !stopped {
            warn!("sandbox did not stop in {:?}", client::STOP_WAIT);
            cli::error!("{}", messages::cli::stop_timeout(STOP_WAIT_SECS));
            return;
        }
        // The sandbox process exits directly after it releases the lock.
        if let Some(started) = started
            && let Ok(Ok(Some(code))) = tokio::time::timeout(EXIT_WAIT, started.child.exit).await
            && code != 0
        {
            warn!("sandbox process exit code {code}");
            cli::error!("{}", messages::cli::SANDBOX_STOP_FAILED);
        }
    }
}

/// A message of the sandbox for the user, for example a warning of the
/// start. The sandbox keeps all its notices until it stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Number of the notice. It grows by one for each notice of the
    /// sandbox, from its boot.
    pub seqno: u64,
    /// Text for the user.
    pub text: String,
}

/// Reads the notices of a sandbox.
#[derive(Clone)]
pub struct NoticeReader(cli_service::Client);

impl NoticeReader {
    /// Make the reader on the CLI service `service`. Tests give the service
    /// of a fake sandbox.
    #[cfg(test)]
    pub fn from_client(service: cli_service::Client) -> Self {
        Self(service)
    }

    /// Get all notices of the sandbox since its boot, oldest first.
    /// Returns:
    ///   The notices. A failure gives no notices and goes only to the log.
    // A sandbox process of an older airlock gives a list of texts. The
    // read of that list fails, and then the client shows no notices.
    pub async fn read(&self) -> Vec<Notice> {
        let response = match self.0.notices_request().send().promise.await {
            Ok(response) => response,
            Err(e) => {
                info!("notices: {e}");
                return Vec::new();
            }
        };
        let Ok(list) = response
            .get()
            .and_then(cli_service::notices_results::Reader::get_notices)
        else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|n| {
                Some(Notice {
                    seqno: n.get_seqno(),
                    text: n.get_text().ok()?.to_str().ok()?.to_string(),
                })
            })
            .collect()
    }
}

/// Adds MCP servers of the host to a sandbox.
#[derive(Clone)]
pub struct McpRegistrar(cli_service::Client);

impl McpRegistrar {
    /// Make the registrar on the CLI service `service`. Tests give the
    /// service of a fake sandbox.
    #[cfg(test)]
    pub fn from_client(service: cli_service::Client) -> Self {
        Self(service)
    }

    /// Add the MCP server `name` of the session `session` to the sandbox.
    /// Args:
    ///  - `session`: Key of the client session that owns the server
    ///  - `name`: Server name
    ///  - `host`: The server on the host
    ///
    /// Returns:
    ///   The grant of the server. Its drop removes the server. Error if the
    ///   sandbox refuses the server or the connection closed.
    pub async fn add(
        &self,
        session: &str,
        name: &str,
        host: mcp_host::Client,
    ) -> capnp::Result<mcp_grant::Client> {
        let mut req = self.0.add_mcp_server_request();
        req.get().set_session(session);
        req.get().set_name(name);
        req.get().set_host(host);
        req.send().promise.await?.get()?.get_grant()
    }
}

/// Stdin server that gives the process the bytes from a channel. Gives end
/// of file when all senders drop.
struct ChannelStdin(RefCell<mpsc::Receiver<Vec<u8>>>);

impl stdin::Server for ChannelStdin {
    // Single-threaded runtime, so RefCell is correct here.
    #[allow(clippy::await_holding_refcell_ref)]
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        let data = self.0.borrow_mut().recv().await;
        let mut frame = results.get().init_input().init_stdin();
        match data {
            Some(data) => frame.set_data(&data),
            None => frame.set_eof(()),
        }
        Ok(())
    }
}
