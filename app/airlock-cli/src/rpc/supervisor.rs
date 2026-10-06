//! Host-side RPC client for the in-VM supervisor.
//!
//! [`Supervisor`] connects over virtio-vsock and exposes typed methods for
//! booting the VM, spawning processes inside it, and a shutdown call for
//! filesystem sync.

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
    pub per_core: Vec<u8>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub load_avg: (f32, f32, f32),
}

/// Host-side daemon specification, serialized into the `boot` RPC. Built
/// from the TOML config after env templates have been expanded.
#[derive(Debug, Clone)]
pub struct DaemonSpec {
    pub name: String,
    pub command: Vec<String>,
    /// `KEY=VALUE` strings, image env already merged in by the builder.
    pub env: Vec<String>,
    pub cwd: String,
    /// Linux signal number for graceful shutdown.
    pub signal: i32,
    /// Milliseconds to wait after sending `signal` before SIGKILL. `0` =
    /// wait forever.
    pub timeout_ms: u32,
    pub restart: RestartPolicy,
    /// Max restart attempts after the initial launch. `0` = no cap.
    pub max_restarts: u32,
    pub harden: bool,
}

/// Host-side directory mask spec, serialised into the boot RPC and
/// applied by guest init. Matches `MaskSpec` in supervisor.capnp.
#[derive(Debug, Clone)]
pub struct MaskSpec {
    pub name: String,
    /// Project-relative paths to mask. Already validated to be plain
    /// relative paths (no leading `/` / `~`, no `..` segments).
    pub paths: Vec<String>,
}

/// Current state of a named daemon, returned by
/// [`Supervisor::poll_daemons`]. `Stopped` and `Killed` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Running,
    Stopped,
    Killed,
}

impl DaemonState {
    pub fn is_terminal(self) -> bool {
        matches!(self, DaemonState::Stopped | DaemonState::Killed)
    }
}

/// The `Supervisor.boot` inputs: VM/mount configuration built from the
/// project config. Carries no process to run — the main process and
/// `airlock exec` both start afterwards via [`Supervisor::spawn`].
pub struct BootRequest<'a> {
    pub project: &'a Project,
    pub vm: &'a VmInstance,
    /// `tracing` filter directive for the guest (see [`crate::cli::LogLevel::filter`]).
    pub log_filter: &'a str,
    /// `(host path, guest path)` socket forwards.
    pub socket_fwds: &'a [(String, String)],
    pub daemons: &'a [DaemonSpec],
    pub masks: &'a [MaskSpec],
    /// The browser bridge; `None` grants no browser.
    pub browser: Option<Browser>,
    /// The clipboard bridge; `None` grants no clipboard.
    pub clipboard: Option<ClipboardImpl>,
}

/// Host-side handle to the in-VM supervisor, wrapping the Cap'n Proto client.
#[derive(Clone)]
pub struct Supervisor {
    supervisor: supervisor::Client,
}

impl Supervisor {
    /// Establish an RPC connection to the supervisor over the given vsock fd.
    /// The returned [`Driver`] runs the connection; the handle works only
    /// while the driver is polled.
    pub fn connect(vsock_fd: OwnedFd) -> anyhow::Result<(Self, Driver)> {
        let transport = vsock_transport(vsock_fd, rpc_twoparty_capnp::Side::Client)?;
        let mut rpc = capnp_rpc::RpcSystem::new(transport, None);
        let client: supervisor::Client = rpc.bootstrap(rpc_twoparty_capnp::Side::Server);
        Ok((Self { supervisor: client }, driver(rpc, "supervisor")))
    }

    /// Clone of the underlying capnp client. Used to hand a late-bound
    /// reference to components like the deny reporter and
    /// [`crate::rpc::guest_network::GuestNetwork`] that need to fire
    /// specific RPCs after the handshake.
    pub fn client(&self) -> supervisor::Client {
        self.supervisor.clone()
    }

    /// A task that pushes the host wall-clock into the guest every
    /// `interval`, forever. VMs have no RTC, so long host sleeps
    /// (laptop lid closed, suspend) cause the guest clock to drift —
    /// breaking TLS validation and every `mtime`-driven build tool.
    /// The RPC is cheap (one UInt64 + UInt32 round-trip) and idempotent;
    /// we just keep re-setting the guest clock to the current host
    /// value.
    pub fn clock_sync(&self, interval: std::time::Duration) -> impl Future<Output = ()> + 'static {
        let supervisor = self.supervisor.clone();
        async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate first tick — the guest's clock was
            // just set by `Supervisor.boot`.
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

    /// Send the `Supervisor.boot()` RPC to bring up the VM: mounts,
    /// networking, daemons. Carries no process to run; the main process
    /// and `airlock exec` both start afterwards via [`Self::spawn`]. The
    /// guest accepts this once per VM.
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

        // Clipboard grant. The capability is only ever built when a
        // direction is granted, so an ungranted sandbox holds a null `sink`
        // and has nothing to call.
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

    /// Start a process inside the booted container — the main process and
    /// `airlock exec` both go through here. Refused before [`Self::boot`]
    /// has succeeded.
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

    /// Sample guest CPU/memory/load for the monitor UI.
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

    /// Request the supervisor to sync filesystems before the VM is destroyed.
    /// `Ok` means the guest confirmed the sync.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let req = self.supervisor.shutdown_request();
        req.send().promise.await?;
        Ok(())
    }

    /// Snapshot of every declared daemon's current state. The guest holds
    /// authoritative state; the host polls during shutdown to drive UI
    /// until all daemons reach a terminal state.
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

    /// Fire-and-forget: ask the supervisor to start graceful shutdown for
    /// every still-running daemon. Follow up with [`Self::poll_daemons`]
    /// until all daemons reach a terminal state.
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
