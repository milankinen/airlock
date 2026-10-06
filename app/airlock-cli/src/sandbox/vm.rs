//! A booted sandbox VM: processes start in it, background services attach
//! to it until the ordered shutdown.

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

/// Upper bound on how long we wait for the guest to stop daemons and flush
/// filesystems during shutdown before forcing the VM down. Generous enough for
/// a healthy guest's sync, but bounded so a wedged guest can't hang the CLI.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// A process to start in the VM (`Supervisor.spawn`). It runs as the image
/// user.
pub struct ProcessSpec {
    /// Command and arguments (the main process: [`super::main_argv`]).
    pub argv: Vec<String>,
    /// Working directory; `None` uses the sandbox cwd.
    pub cwd: Option<String>,
    /// Variables layered over the sandbox env (see [`util::merge_env`]).
    pub env: Vec<(String, String)>,
    /// Variables removed from the resulting env.
    pub unset: Vec<String>,
    pub stdin: stdin::Client,
    /// Terminal size for PTY mode; `None` runs it with pipes.
    pub pty: PtySize,
}

/// What is left after a shutdown.
pub struct Stopped {
    /// The project. The caller still holds the [`crate::project::SandboxLock`]
    /// it opened the project with; drop that to release the sandbox lock.
    pub project: Project,
    /// The guest confirmed its filesystem sync before the VM stopped.
    pub synced: bool,
}

/// A booted VM ([`super::boot::boot`]). No process runs in it until the
/// caller spawns one.
pub struct Vm {
    project: Project,
    instance: VmInstance,
    supervisor: rpc::Supervisor,
    network: NetworkHandle,
    /// Names of the daemons the boot started, stopped (with a progress
    /// line each) during shutdown.
    daemon_names: Vec<String>,
    tasks: BootTasks,
}

impl Vm {
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

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Live network control and events, for the runtime.
    pub fn network(&self) -> &NetworkHandle {
        &self.network
    }

    pub fn supervisor(&self) -> &rpc::Supervisor {
        &self.supervisor
    }

    /// The guest's loopback, for services that forward host ports into it.
    pub fn guest_network(&self) -> GuestNetwork {
        GuestNetwork::new(self.supervisor.client())
    }

    /// Start `spec` in the VM, with the sandbox env plus the spec's
    /// overrides. The caller drives the returned process.
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

    /// Serve `airlock exec` on the sandbox's `cli.sock` until shutdown,
    /// which unlinks the socket. Exec'd processes get the sandbox env with
    /// the client's overrides layered on top.
    pub fn serve_cli(&mut self) -> anyhow::Result<()> {
        let sock_path = crate::cache::cli_sock_path(&self.project.sandbox_dir)?;
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

    /// Stop everything the boot started, in order:
    ///
    /// 1. the services (their ports and `cli.sock` are free afterwards);
    /// 2. the daemons, then the guest filesystem sync, together bounded by
    ///    [`SHUTDOWN_TIMEOUT`];
    /// 3. the VM, which must confirm it stopped;
    /// 4. the RPC transport.
    ///
    /// Returns the project to the caller, which still holds the
    /// [`crate::project::SandboxLock`] it opened the project with. An
    /// unconfirmed VM stop is an error; the rest of the teardown still runs.
    pub async fn shutdown(mut self) -> anyhow::Result<Stopped> {
        self.tasks.stop_services().await;

        // Give the guest a bounded chance to stop its daemons and flush
        // filesystems, then tear the VM down regardless. A wedged guest must
        // not hang shutdown forever — that previously left the user resorting
        // to SIGKILL, which skips the VM's Drop and orphans cloud-hypervisor /
        // virtiofsd plus a stale lock file.
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
