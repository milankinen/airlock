//! Booted sandbox VM.
//!
//! A running sandbox VM. Processes start in it, and background services
//! attach to it until the ordered shutdown.

use std::time::Duration;

use airlock_common::supervisor_capnp::stdin;
use anyhow::Context;
use tracing::info;

use super::tasks::BootTasks;
use crate::network::NetworkHandle;
use crate::project::Project;
use crate::rpc::guest_network::GuestNetwork;
use crate::runtime::{self, PtySize, SignalStream};
use crate::vm::VmInstance;
use crate::{cli_server, daemon, rpc, util};

/// Maximum time for the guest to stop daemons and flush filesystems during
/// shutdown. After it, the VM stops by force. The time is sufficient for the
/// sync of a healthy guest. The limit prevents a hung guest from blocking the
/// CLI.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// The project. The caller still holds the [`crate::project::SandboxLock`]
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

    /// Get the project of the VM.
    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Get the live network control and events, for the runtime.
    pub fn network(&self) -> &NetworkHandle {
        &self.network
    }

    /// Get the supervisor RPC client.
    pub fn supervisor(&self) -> &rpc::Supervisor {
        &self.supervisor
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
            anyhow::bail!("spawn: empty command");
        };
        let env = util::merge_env(&self.instance.env, env, &unset);
        let cwd = cwd.unwrap_or_else(|| self.instance.cwd.clone());
        self.supervisor
            .spawn(stdin, pty, cmd, args, &cwd, &env)
            .await
    }

    /// Serve `airlock exec` on the `cli.sock` of the sandbox until shutdown.
    /// The shutdown removes the socket. The exec processes get the sandbox
    /// env with the overrides of the client.
    pub fn serve_cli(&mut self) -> anyhow::Result<()> {
        let sock_path =
            crate::cache::cli_sock_path(&self.project.context.data_dir, &self.project.sandbox_dir)?;
        let base_env = self.instance.env.clone();
        let server = cli_server::serve(sock_path, self.supervisor.clone(), base_env);
        self.tasks.spawn_service(server);
        Ok(())
    }

    /// Forward host signals from `signals` to `proc` until shutdown.
    pub fn forward_signals(&mut self, signals: SignalStream, proc: rpc::Process) {
        self.tasks
            .spawn_service(runtime::forward_signals(signals, proc));
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
    ///   [`crate::project::SandboxLock`] of the project. Error if the VM stop
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
        stopped.context("VM stop not confirmed")?;
        Ok(Stopped {
            project: self.project,
            synced,
        })
    }
}
