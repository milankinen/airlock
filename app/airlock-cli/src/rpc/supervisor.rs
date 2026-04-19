//! Host-side RPC client for the in-VM supervisor.
//!
//! [`Supervisor`] connects over virtio-vsock and exposes typed methods for
//! starting and exec'ing processes, plus a shutdown call for filesystem sync.

use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};

use airlock_common::supervisor_capnp::*;

use crate::network::Network;
use crate::project::Project;
use crate::rpc::logging::LogSinkImpl;
use crate::rpc::process::Process;

/// Snapshot of guest resource usage returned by [`Supervisor::poll_stats`].
#[derive(Debug, Clone, Default)]
pub struct StatsSnapshot {
    pub per_core: Vec<u8>,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub load_avg: (f32, f32, f32),
}

/// Host-side handle to the in-VM supervisor, wrapping the Cap'n Proto client.
#[derive(Clone)]
pub struct Supervisor {
    supervisor: supervisor::Client,
}

impl Supervisor {
    /// Establish an RPC connection to the supervisor over the given vsock fd.
    pub fn connect(vsock_fd: OwnedFd) -> anyhow::Result<Self> {
        use futures::AsyncReadExt;

        #[cfg(target_os = "macos")]
        let stream = {
            let std_stream = unsafe { std::net::TcpStream::from_raw_fd(vsock_fd.into_raw_fd()) };
            std_stream.set_nonblocking(true)?;
            tokio::net::TcpStream::from_std(std_stream)?
        };
        #[cfg(target_os = "linux")]
        let stream = {
            let std_stream =
                unsafe { std::os::unix::net::UnixStream::from_raw_fd(vsock_fd.into_raw_fd()) };
            std_stream.set_nonblocking(true)?;
            tokio::net::UnixStream::from_std(std_stream)?
        };
        let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

        let network = capnp_rpc::twoparty::VatNetwork::new(
            reader,
            writer,
            capnp_rpc::rpc_twoparty_capnp::Side::Client,
            capnp::message::ReaderOptions::default(),
        );

        let mut rpc = capnp_rpc::RpcSystem::new(Box::new(network), None);
        let client: supervisor::Client = rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

        tokio::task::spawn_local(rpc);

        Ok(Self { supervisor: client })
    }

    /// Send the initial `Supervisor.start()` RPC to bootstrap the VM and
    /// launch the main container process. Returns a [`Process`] handle for
    /// polling output and forwarding signals.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        &self,
        args: &crate::cli::CliArgs,
        project: &Project,
        vm: &crate::vm::VmInstance,
        stdin: stdin::Client,
        pty_size: Option<(u16, u16)>,
        network: Network,
        epoch: u64,
        epoch_nanos: u32,
    ) -> anyhow::Result<Process> {
        let log_sink: log_sink::Client = capnp_rpc::new_client(LogSinkImpl);

        // Collect socket forwards before network is moved into the RPC capability.
        let socket_fwds: Vec<(String, String)> = network
            .socket_map
            .iter()
            .map(|(guest, host)| (host.to_string_lossy().into_owned(), guest.clone()))
            .collect();

        let mut req = self.supervisor.start_request();
        req.get().set_stdin(stdin);
        if let Some((rows, cols)) = pty_size {
            let mut size = req.get().init_pty().init_size();
            size.set_rows(rows);
            size.set_cols(cols);
        } else {
            req.get().init_pty().set_none(());
        }
        req.get().set_network(capnp_rpc::new_client(network));
        req.get().set_logs(log_sink);
        req.get().set_log_filter(args.log_filter());

        // Init config: epoch, host ports
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

        // Process configuration
        req.get()
            .set_cmd(vm.cmd.first().map_or("/bin/sh", String::as_str));
        let proc_args = if vm.cmd.len() > 1 { &vm.cmd[1..] } else { &[] };
        let mut args_b = req.get().init_args(proc_args.len() as u32);
        for (i, a) in proc_args.iter().enumerate() {
            args_b.set(i as u32, a);
        }
        let mut env_b = req.get().init_env(vm.env.len() as u32);
        for (i, e) in vm.env.iter().enumerate() {
            env_b.set(i as u32, e);
        }
        req.get().set_cwd(&vm.cwd);
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

        let response = req.send().promise.await?;
        let proc = response.get()?.get_proc()?;

        Ok(Process::new(proc))
    }

    /// Attach a new process to the running container.
    pub async fn exec(
        &self,
        stdin: stdin::Client,
        pty_size: Option<(u16, u16)>,
        cmd: &str,
        args: &[String],
        cwd: &str,
        env: &[String],
    ) -> anyhow::Result<Process> {
        let mut req = self.supervisor.exec_request();
        req.get().set_stdin(stdin);
        if let Some((rows, cols)) = pty_size {
            let mut size = req.get().init_pty().init_size();
            size.set_rows(rows);
            size.set_cols(cols);
        } else {
            req.get().init_pty().set_none(());
        }
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
    pub async fn shutdown(&self) {
        let req = self.supervisor.shutdown_request();
        if let Err(e) = req.send().promise.await {
            tracing::debug!("shutdown RPC: {e}");
        }
    }
}
