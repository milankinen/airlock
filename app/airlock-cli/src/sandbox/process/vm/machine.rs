//! Booted sandbox VM.
//!
//! A running sandbox VM. Processes start in it, and background services
//! attach to it until the ordered shutdown.

use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::stdin;
use anyhow::Context;
use tracing::info;

use super::tasks::BootTasks;
use crate::approval::MonitorSlot;
use crate::network::NetworkHandle;
use crate::rpc::guest_network::GuestNetwork;
use crate::runtime::PtySize;
use crate::sandbox::Project;
use crate::sandbox::process::server as cli_server;
use crate::sandbox::process::server::connections::ConnectionTracker;
use crate::sandbox::process::server::monitor::MonitorFeed;
use crate::vm::VmInstance;
use crate::{daemon, rpc, util};

/// Maximum time for the guest to stop daemons and flush filesystems during
/// shutdown. After it, the VM stops by force. The time is sufficient for the
/// sync of a healthy guest. The limit prevents a hung guest from blocking the
/// CLI.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// A process to start in the VM (`Supervisor.spawn`). It runs as the image
/// user.
pub struct ProcessSpec {
    /// Command and arguments (the main process: [`super::main_argv`]).
    pub argv: Vec<String>,
    /// Working directory. `None` uses the sandbox cwd.
    pub cwd: Option<String>,
    /// Variables that override the sandbox env (see [`util::merge_env`]).
    pub env: Vec<(String, String)>,
    /// Variables removed from the resulting env.
    pub unset: Vec<String>,
    /// Stdin of the process.
    pub stdin: stdin::Client,
    /// Terminal size for PTY mode. `None` runs the process with pipes.
    pub pty: PtySize,
}

/// Result of a shutdown.
pub struct Stopped {
    /// The project. The caller still holds the [`crate::sandbox::SandboxLock`]
    /// that it used to open the project. Drop that lock to release the
    /// sandbox.
    pub project: Project,
    /// `true` if the guest confirmed its filesystem sync before the VM
    /// stopped.
    pub synced: bool,
}

/// A booted VM (see [`super::boot::boot`]). No process runs in it until the
/// caller starts one.
pub struct Vm {
    project: Project,
    instance: VmInstance,
    supervisor: rpc::Supervisor,
    network: NetworkHandle,
    /// Names of the daemons that the boot started. The shutdown stops them
    /// and shows one progress line for each.
    daemon_names: Vec<String>,
    tasks: BootTasks,
}

impl Vm {
    /// Make a [`Vm`] from the parts of a completed boot.
    pub(super) fn new(
        project: Project,
        instance: VmInstance,
        supervisor: rpc::Supervisor,
        network: NetworkHandle,
        daemon_names: Vec<String>,
        tasks: BootTasks,
    ) -> Self {
        Self {
            project,
            instance,
            supervisor,
            network,
            daemon_names,
            tasks,
        }
    }

    /// Get the guest loopback network, for services that forward host ports
    /// to it.
    pub fn guest_network(&self) -> GuestNetwork {
        GuestNetwork::new(self.supervisor.client())
    }

    /// Start a process in the VM. It gets the sandbox env with the overrides
    /// of `spec`.
    /// Returns:
    ///   The started process, or error if the command is empty or the spawn
    ///   fails. The caller drives the process.
    pub async fn spawn(&self, spec: ProcessSpec) -> anyhow::Result<rpc::Process> {
        let ProcessSpec {
            argv,
            cwd,
            env,
            unset,
            stdin,
            pty,
        } = spec;
        let Some((cmd, args)) = argv.split_first() else {
            anyhow::bail!(crate::messages::sandbox::SPAWN_EMPTY_COMMAND);
        };
        let env = util::merge_env(&self.instance.env, env, &unset);
        let cwd = cwd.unwrap_or_else(|| self.instance.cwd.clone());
        self.supervisor
            .spawn(stdin, pty, cmd, args, &cwd, &env)
            .await
    }

    /// Serve `airlock exec`, `airlock network forward` and the monitor feed
    /// on the `cli.sock` of the sandbox until shutdown. The shutdown removes
    /// the socket. The exec processes get the sandbox env with the overrides
    /// of the client.
    /// Args:
    ///  - `tracker`: Connection count of the sandbox
    ///  - `notices`: Notice log of the sandbox for the clients
    ///  - `approvals`: Place of the monitor dialog for approval requests.
    ///    It holds the dialog of the attached monitor client.
    ///  - `first`: Connected socket of the first client. It counts at once,
    ///    before the socket file exists
    pub fn serve_cli(
        &mut self,
        tracker: Rc<ConnectionTracker>,
        notices: crate::sandbox::NoticeLog,
        approvals: MonitorSlot,
        first: Option<std::os::unix::net::UnixStream>,
    ) -> anyhow::Result<()> {
        let sock_path =
            crate::cache::cli_sock_path(&self.project.context.data, &self.project.sandbox_dir)?;
        let defaults = cli_server::ExecDefaults {
            env: self.instance.env.clone(),
            cwd: self.instance.cwd.clone(),
        };
        let forwards =
            cli_server::ForwardRules::new(&self.project.config.network, self.network.control());
        // Count the first client now, not in the server task. Thus the count
        // is not 0 when the server starts.
        let first = match first {
            Some(stream) => {
                stream.set_nonblocking(true)?;
                let stream = tokio::net::UnixStream::from_std(stream)?;
                tracker
                    .open()
                    .map(|hold| cli_server::FirstClient { stream, hold })
            }
            None => None,
        };
        let server = cli_server::serve(
            sock_path,
            self.supervisor.clone(),
            defaults,
            forwards,
            tracker,
            notices,
            MonitorFeed {
                network: self.network.clone(),
                project_path: self.project.host_cwd.to_string(),
                approvals,
            },
            first,
        );
        self.tasks.spawn_service(server);
        Ok(())
    }

    /// Stop all that the boot started, in this order:
    ///
    /// 1. The services. Their ports and `cli.sock` are free after this step.
    /// 2. The daemons, then the guest filesystem sync. The two together have
    ///    the time limit [`SHUTDOWN_TIMEOUT`].
    /// 3. The VM, which must confirm that it stopped.
    /// 4. The RPC transport.
    ///
    /// Returns:
    ///   The project, for the caller that still holds the
    ///   [`crate::sandbox::SandboxLock`] of the project. Error if the VM stop
    ///   is not confirmed. All other shutdown steps still run.
    pub async fn shutdown(mut self) -> anyhow::Result<Stopped> {
        self.tasks.stop_services().await;

        // Give the guest limited time to stop its daemons and flush
        // filesystems. Then stop the VM in all cases. A hung guest must not
        // block the shutdown forever. Without the limit, users must send
        // SIGKILL, which skips the VM's Drop. That leaves orphan
        // cloud-hypervisor and virtiofsd processes and a stale lock file.
        let supervisor = &self.supervisor;
        let daemon_names = &self.daemon_names;
        let graceful = async {
            if !daemon_names.is_empty() {
                info!("daemon shutdown");
                daemon::run_shutdown(supervisor, daemon_names).await;
            }
            // Sync filesystems before stopping the VM.
            info!("supervisor shutdown");
            supervisor.shutdown().await
        };
        let synced = match tokio::time::timeout(SHUTDOWN_TIMEOUT, graceful).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                info!("guest sync failed: {e}");
                false
            }
            Err(_) => {
                info!("guest shutdown timed out after {SHUTDOWN_TIMEOUT:?}; forcing VM teardown");
                false
            }
        };

        // Drain file-sync events, then stop the VM.
        info!("vm shutdown");
        let stopped = self.instance.shutdown().await;
        self.tasks.stop_transport().await;
        stopped.context(crate::messages::sandbox::VM_STOP_NOT_CONFIRMED)?;
        Ok(Stopped {
            project: self.project,
            synced,
        })
    }
}
