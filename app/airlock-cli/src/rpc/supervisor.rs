//! Host-side control of the supervisor in the VM.
//!
//! Lets the host boot the sandbox, start processes in it, read resource
//! statistics, control background daemons and shut the VM down.

use std::future::Future;
use std::os::unix::io::OwnedFd;

use airlock_common::supervisor_capnp::{
    DaemonState as WireDaemonState, RestartPolicy as WireRestartPolicy, *,
};
use capnp_rpc::rpc_twoparty_capnp;

use crate::config::config_values::RestartPolicy;
use crate::project::Project;
use crate::rpc::browser::{Browser, BrowserImpl};
use crate::rpc::clipboard::ClipboardImpl;
use crate::rpc::logging::LogSinkImpl;
use crate::rpc::process::Process;
use crate::rpc::{Driver, driver, vsock_transport};
use crate::vm::VmInstance;

/// Snapshot of guest resource usage returned by [`Supervisor::poll_stats`].
#[derive(Debug, Clone, Default)]
pub struct StatsSnapshot {
    /// CPU utilization of each core, 0..100.
    pub per_core: Vec<u8>,
    /// Total guest memory in bytes.
    pub total_bytes: u64,
    /// Used guest memory in bytes.
    pub used_bytes: u64,
    /// Load average over 1, 5 and 15 minutes.
    pub load_avg: (f32, f32, f32),
}

/// Host-side daemon specification for the `boot` RPC. Made from the TOML
/// config after the env templates are expanded.
#[derive(Debug, Clone)]
pub struct DaemonSpec {
    /// Daemon name.
    pub name: String,
    /// `argv[0]` and the arguments.
    pub command: Vec<String>,
    /// `KEY=VALUE` strings. The builder already merged the image env.
    pub env: Vec<String>,
    /// Working directory in the guest.
    pub cwd: String,
    /// Linux signal number for graceful shutdown.
    pub signal: i32,
    /// Milliseconds to wait after `signal` before SIGKILL. `0` = wait
    /// forever.
    pub timeout_ms: u32,
    /// When the guest restarts the daemon.
    pub restart: RestartPolicy,
    /// Maximum number of restarts after the first start. `0` = no limit.
    pub max_restarts: u32,
    /// Hardening of this daemon. Independent of the main-shell setting.
    pub harden: bool,
}

/// Host-side directory mask specification for the boot RPC. Guest init
/// applies it. Matches `MaskSpec` in supervisor.capnp.
#[derive(Debug, Clone)]
pub struct MaskSpec {
    /// Mask name.
    pub name: String,
    /// Project-relative paths to mask. Already validated to be plain
    /// relative paths (no leading `/` or `~`, no `..` segments).
    pub paths: Vec<String>,
}

/// Current state of a named daemon, returned by
/// [`Supervisor::poll_daemons`]. `Stopped` and `Killed` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// Alive, or between restarts.
    Running,
    /// Terminated cleanly (shutdown, max restarts, or clean exit with the
    /// on-failure policy).
    Stopped,
    /// Killed with SIGKILL after the graceful-shutdown timeout.
    Killed,
}

impl DaemonState {
    /// Check if the state is terminal (`Stopped` or `Killed`).
    pub fn is_terminal(self) -> bool {
        matches!(self, DaemonState::Stopped | DaemonState::Killed)
    }
}

/// Inputs of `Supervisor.boot`: VM and mount configuration from the
/// project config. It has no process to run. The main process and
/// `airlock exec` both start later with [`Supervisor::spawn`].
pub struct BootRequest<'a> {
    /// Project to boot.
    pub project: &'a Project,
    /// Prepared VM instance (image, mounts, caches, user ids).
    pub vm: &'a VmInstance,
    /// `tracing` filter directive for the guest (see
    /// [`crate::cli::LogLevel::filter`]).
    pub log_filter: &'a str,
    /// `(host path, guest path)` socket forwards.
    pub socket_fwds: &'a [(String, String)],
    /// Daemons that the guest starts.
    pub daemons: &'a [DaemonSpec],
    /// Directory masks that guest init applies.
    pub masks: &'a [MaskSpec],
    /// Browser bridge. `None` grants no browser.
    pub browser: Option<Browser>,
    /// Clipboard bridge. `None` grants no clipboard.
    pub clipboard: Option<ClipboardImpl>,
}

/// Host-side handle to the in-VM supervisor. Wraps the Cap'n Proto client.
#[derive(Clone)]
pub struct Supervisor {
    supervisor: supervisor::Client,
}

impl Supervisor {
    /// Connect to the supervisor over a vsock socket.
    /// Args:
    ///  - `vsock_fd`: Connected vsock socket of the supervisor channel
    ///
    /// Returns:
    ///   Handle and the [`Driver`] that runs the connection. The handle
    ///   works only while the driver is polled.
    pub fn connect(vsock_fd: OwnedFd) -> anyhow::Result<(Self, Driver)> {
        let transport = vsock_transport(vsock_fd, rpc_twoparty_capnp::Side::Client)?;
        let mut rpc = capnp_rpc::RpcSystem::new(transport, None);
        let client: supervisor::Client = rpc.bootstrap(rpc_twoparty_capnp::Side::Server);
        Ok((Self { supervisor: client }, driver(rpc, "supervisor")))
    }

    /// Get a clone of the capnp client. Components that send specific RPCs
    /// after the handshake use it, for example the deny reporter and
    /// [`crate::rpc::guest_network::GuestNetwork`].
    pub fn client(&self) -> supervisor::Client {
        self.supervisor.clone()
    }

    /// Make a task that sets the guest clock to the host wall-clock every
    /// `interval`, forever.
    ///
    /// VMs have no RTC. Thus long host sleeps (laptop lid closed, suspend)
    /// cause the guest clock to drift. This breaks TLS validation and all
    /// `mtime`-driven build tools.
    pub fn clock_sync(&self, interval: std::time::Duration) -> impl Future<Output = ()> + 'static {
        // The RPC is cheap (one UInt64 + UInt32 round trip) and idempotent,
        // so the task sets the guest clock again on each tick.
        let supervisor = self.supervisor.clone();
        async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate first tick: `Supervisor.boot` just set the
            // guest clock.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let (epoch, nanos) = now();
                let mut req = supervisor.sync_clock_request();
                req.get().set_epoch(epoch);
                req.get().set_epoch_nanos(nanos);
                if let Err(e) = req.send().promise.await {
                    tracing::debug!("clock sync: {e}");
                }
            }
        }
    }

    /// Send the `Supervisor.boot()` RPC to start the VM: mounts, network,
    /// daemons. It has no process to run. The main process and
    /// `airlock exec` both start later with [`Self::spawn`]. The guest
    /// accepts this call one time per VM.
    pub async fn boot(&self, boot: BootRequest<'_>) -> anyhow::Result<()> {
        let BootRequest {
            project,
            vm,
            log_filter,
            socket_fwds,
            daemons,
            masks,
            browser,
            clipboard,
        } = boot;
        let log_sink: log_sink::Client = capnp_rpc::new_client(LogSinkImpl);

        let mut req = self.supervisor.boot_request();
        req.get().set_logs(log_sink);
        req.get().set_log_filter(log_filter);

        // Init config: epoch, host ports
        let (epoch, epoch_nanos) = now();
        req.get().set_epoch(epoch);
        req.get().set_epoch_nanos(epoch_nanos);
        let port_forwards =
            crate::network::rules::port_forwards_from_config(&project.config.network);
        let guest_ports: Vec<u16> = port_forwards.iter().map(|(g, _)| *g).collect();
        let mut hp_builder = req.get().init_host_ports(guest_ports.len() as u32);
        for (i, port) in guest_ports.iter().enumerate() {
            hp_builder.set(i as u32, *port);
        }

        // Socket forwards
        let mut sf_builder = req.get().init_sockets(socket_fwds.len() as u32);
        for (i, (host, guest)) in socket_fwds.iter().enumerate() {
            sf_builder.reborrow().get(i as u32).set_host(host);
            sf_builder.reborrow().get(i as u32).set_guest(guest);
        }

        req.get().set_uid(vm.uid);
        req.get().set_gid(vm.gid);
        req.get().set_nested_virt(project.config.vm.kvm);
        req.get().set_harden(project.config.vm.harden);

        // Mount configuration
        req.get().set_image_id(&vm.image_id);
        let mut layers_b = req.get().init_image_layers(vm.image_layers.len() as u32);
        for (i, d) in vm.image_layers.iter().enumerate() {
            layers_b.set(i as u32, d);
        }
        req.get().set_ca_cert(project.ca_cert.as_bytes());

        let dirs: Vec<_> = vm
            .mounts
            .iter()
            .filter(|m| matches!(m.mount_type, crate::vm::mount::MountType::Dir { .. }))
            .collect();
        let mut dirs_b = req.get().init_dirs(dirs.len() as u32);
        for (i, m) in dirs.iter().enumerate() {
            dirs_b.reborrow().get(i as u32).set_tag(m.key());
            dirs_b.reborrow().get(i as u32).set_target(&m.target);
            dirs_b.reborrow().get(i as u32).set_read_only(m.read_only);
        }

        let files: Vec<_> = vm
            .mounts
            .iter()
            .filter(|m| matches!(m.mount_type, crate::vm::mount::MountType::File { .. }))
            .collect();
        let mut files_b = req.get().init_files(files.len() as u32);
        for (i, m) in files.iter().enumerate() {
            files_b.reborrow().get(i as u32).set_target(&m.target);
            files_b.reborrow().get(i as u32).set_read_only(m.read_only);
            files_b.reborrow().get(i as u32).set_key(m.key());
        }

        let mut caches_b = req.get().init_caches(vm.caches.len() as u32);
        for (i, (name, enabled, paths)) in vm.caches.iter().enumerate() {
            caches_b.reborrow().get(i as u32).set_name(name);
            caches_b.reborrow().get(i as u32).set_enabled(*enabled);
            let mut paths_b = caches_b
                .reborrow()
                .get(i as u32)
                .init_paths(paths.len() as u32);
            for (j, p) in paths.iter().enumerate() {
                paths_b.set(j as u32, p);
            }
        }

        let mut daemons_b = req.get().init_daemons(daemons.len() as u32);
        for (i, d) in daemons.iter().enumerate() {
            let mut entry = daemons_b.reborrow().get(i as u32);
            entry.set_name(d.name.as_str());
            let mut command_b = entry.reborrow().init_command(d.command.len() as u32);
            for (j, c) in d.command.iter().enumerate() {
                command_b.set(j as u32, c.as_str());
            }
            let mut env_b = entry.reborrow().init_env(d.env.len() as u32);
            for (j, e) in d.env.iter().enumerate() {
                env_b.set(j as u32, e.as_str());
            }
            entry.set_cwd(d.cwd.as_str());
            entry.set_signal(d.signal);
            entry.set_timeout_ms(d.timeout_ms);
            entry.set_restart(match d.restart {
                RestartPolicy::Always => WireRestartPolicy::Always,
                RestartPolicy::OnFailure => WireRestartPolicy::OnFailure,
            });
            entry.set_max_restarts(d.max_restarts);
            entry.set_harden(d.harden);
        }

        let mut masks_b = req.get().init_masks(masks.len() as u32);
        for (i, m) in masks.iter().enumerate() {
            let mut entry = masks_b.reborrow().get(i as u32);
            entry.set_name(m.name.as_str());
            let mut paths_b = entry.reborrow().init_paths(m.paths.len() as u32);
            for (j, p) in m.paths.iter().enumerate() {
                paths_b.set(j as u32, p.as_str());
            }
        }

        // Clipboard grant. The capability exists only when a direction is
        // granted. Otherwise the sandbox has a null `sink` and nothing to
        // call.
        if let Some(grant) = clipboard {
            let mut b = req.get().init_clipboard();
            b.set_copy(grant.copy);
            b.set_paste(grant.paste);
            b.set_limit(grant.limit);
            let sink: clipboard::Client = capnp_rpc::new_client(grant);
            b.set_sink(sink);
        }

        // Browser grant: only boots with a network service have one.
        if let Some(browser) = browser {
            let sink: browser::Client = capnp_rpc::new_client(BrowserImpl::new(browser));
            req.get().init_browser().set_sink(sink);
        }

        req.send().promise.await?;
        Ok(())
    }

    /// Start a process in the booted container. The main process and
    /// `airlock exec` both use it. The guest refuses it before a successful
    /// [`Self::boot`].
    /// Args:
    ///  - `stdin`: Stdin capability of the process (see
    ///    [`Stdin`](crate::rpc::Stdin))
    ///  - `pty_size`: Terminal size `(rows, cols)` for PTY mode, or `None`
    ///    for pipe mode
    ///  - `cmd`: Program to run
    ///  - `args`: Program arguments
    ///  - `cwd`: Working directory in the guest
    ///  - `env`: `KEY=VALUE` environment strings
    ///
    /// Returns:
    ///   Handle of the started process.
    pub async fn spawn(
        &self,
        stdin: stdin::Client,
        pty_size: Option<(u16, u16)>,
        cmd: &str,
        args: &[String],
        cwd: &str,
        env: &[String],
    ) -> anyhow::Result<Process> {
        let mut req = self.supervisor.spawn_request();
        req.get().set_stdin(stdin);
        super::set_pty(req.get().init_pty(), pty_size);
        req.get().set_cmd(cmd);
        let mut args_b = req.get().init_args(args.len() as u32);
        for (i, a) in args.iter().enumerate() {
            args_b.set(i as u32, a.as_str());
        }
        req.get().set_cwd(cwd);
        let mut env_b = req.get().init_env(env.len() as u32);
        for (i, e) in env.iter().enumerate() {
            env_b.set(i as u32, e.as_str());
        }
        let response = req.send().promise.await?;
        Ok(Process::new(response.get()?.get_proc()?))
    }

    /// Read the guest CPU, memory and load for the monitor UI.
    pub async fn poll_stats(&self) -> anyhow::Result<StatsSnapshot> {
        let req = self.supervisor.poll_stats_request();
        let response = req.send().promise.await?;
        let snap = response.get()?.get_snapshot()?;

        let cpu = snap.get_cpu()?;
        let per_core: Vec<u8> = cpu.get_per_core()?.iter().collect();

        let mem = snap.get_memory()?;
        let total_bytes = mem.get_total_bytes();
        let used_bytes = mem.get_used_bytes();

        let la = snap.get_load_average()?;

        Ok(StatsSnapshot {
            per_core,
            total_bytes,
            used_bytes,
            load_avg: (la.get_one(), la.get_five(), la.get_fifteen()),
        })
    }

    /// Ask the supervisor to sync the filesystems before the VM stops.
    /// `Ok` means that the guest confirmed the sync.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let req = self.supervisor.shutdown_request();
        req.send().promise.await?;
        Ok(())
    }

    /// Get the current state of each declared daemon. The guest has the
    /// authoritative state. During shutdown, the host polls it to update
    /// the UI until all daemons are in a terminal state.
    pub async fn poll_daemons(&self) -> anyhow::Result<Vec<(String, DaemonState)>> {
        let req = self.supervisor.poll_daemons_request();
        let response = req.send().promise.await?;
        let states = response.get()?.get_states()?;
        let mut out = Vec::with_capacity(states.len() as usize);
        for entry in states {
            let name = entry.get_name()?.to_str()?.to_string();
            let state = match entry.get_state()? {
                WireDaemonState::Running => DaemonState::Running,
                WireDaemonState::Stopped => DaemonState::Stopped,
                WireDaemonState::Killed => DaemonState::Killed,
            };
            out.push((name, state));
        }
        Ok(out)
    }

    /// Ask the supervisor to start a graceful shutdown of each running
    /// daemon. Does not wait for the result. After this call, use
    /// [`Self::poll_daemons`] until all daemons are in a terminal state.
    pub async fn shutdown_daemons(&self) {
        let req = self.supervisor.shutdown_daemons_request();
        if let Err(e) = req.send().promise.await {
            tracing::debug!("shutdown_daemons RPC: {e}");
        }
    }
}

/// Host wall-clock as `(seconds, nanoseconds)` since the Unix epoch.
fn now() -> (u64, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs(), now.subsec_nanos())
}
