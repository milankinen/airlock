//! Host-side RPC client for the in-VM supervisor.
//!
//! [`Supervisor`] connects over virtio-vsock and exposes typed methods for
//! starting and exec'ing processes, plus a shutdown call for filesystem sync.

use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};

use ezpez_protocol::supervisor_capnp::*;

use crate::cli::CliArgs;

/// Build the command + args for the supervisor to execute.
/// With the `dev` feature and `EZ_DEV_NO_CRUN=true`, runs a shell instead of crun.
fn build_command() -> (String, Vec<String>) {
    #[cfg(feature = "dev")]
    if std::env::var("EZ_DEV_NO_CRUN").is_ok_and(|v| v == "true" || v == "1") {
        tracing::debug!("dev mode: skipping crun, starting shell");
        return ("/bin/sh".to_string(), vec![]);
    }

    (
        "crun".to_string(),
        vec![
            "run".to_string(),
            "--no-pivot".to_string(),
            "--bundle".to_string(),
            "/mnt/overlay".to_string(),
            "ezpez0".to_string(),
        ],
    )
}

/// Build the `crun exec` invocation for attaching to the running container.
/// In dev mode (`EZ_DEV_NO_CRUN=true`) runs the command directly instead.
pub fn build_exec_command(
    user_cmd: &str,
    user_args: &[String],
    cwd: &str,
    env: &[String],
    pty: bool,
) -> (String, Vec<String>) {
    #[cfg(feature = "dev")]
    if std::env::var("EZ_DEV_NO_CRUN").is_ok_and(|v| v == "true" || v == "1") {
        tracing::debug!("dev mode: skipping crun exec, running directly");
        return (user_cmd.to_string(), user_args.to_vec());
    }

    let mut args = vec!["exec".to_string()];
    if pty {
        args.push("--tty".to_string());
    }
    if !cwd.is_empty() && cwd != "/" {
        args.push("--cwd".to_string());
        args.push(cwd.to_string());
    }
    for e in env {
        args.push("--env".to_string());
        args.push(e.clone());
    }
    args.push("ezpez0".to_string());
    args.push(user_cmd.to_string());
    args.extend_from_slice(user_args);

    ("crun".to_string(), args)
}
use crate::network::Network;
use crate::project::Project;
use crate::rpc::logging::LogSinkImpl;
use crate::rpc::process::Process;
use crate::rpc::stdin::Stdin;

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
    pub async fn start(
        &self,
        args: &CliArgs,
        project: &Project,
        stdin: Stdin,
        network: Network,
        epoch: u64,
    ) -> anyhow::Result<Process> {
        let log_sink: log_sink::Client = capnp_rpc::new_client(LogSinkImpl);

        // Collect socket forwards before network is moved into the RPC capability.
        let socket_fwds: Vec<(String, String)> = network
            .socket_map
            .iter()
            .map(|(guest, host)| (host.to_string_lossy().into_owned(), guest.clone()))
            .collect();

        let mut req = self.supervisor.start_request();
        let pty_size = stdin.pty_size();
        req.get().set_stdin(capnp_rpc::new_client(stdin));
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

        // Build the command for the supervisor to execute
        let (cmd, cmd_args) = build_command();
        req.get().set_cmd(&cmd);
        let mut args_builder = req.get().init_args(cmd_args.len() as u32);
        for (i, arg) in cmd_args.iter().enumerate() {
            args_builder.set(i as u32, arg);
        }

        // TLS passthrough hosts (cert pinning — skip MITM)
        let tls_passthrough =
            crate::network::rules::tls_passthrough_from_config(&project.config.network);
        let mut pt_builder = req.get().init_tls_passthrough(tls_passthrough.len() as u32);
        for (i, host) in tls_passthrough.iter().enumerate() {
            pt_builder.set(i as u32, host.as_str());
        }

        // Init config: epoch, host ports
        req.get().set_epoch(epoch);
        let host_ports =
            crate::network::rules::localhost_ports_from_config(&project.config.network);
        let mut hp_builder = req.get().init_host_ports(host_ports.len() as u32);
        for (i, port) in host_ports.iter().enumerate() {
            hp_builder.set(i as u32, *port);
        }

        // Socket forwards — already expanded, sourced from network.socket_map
        let mut sf_builder = req.get().init_sockets(socket_fwds.len() as u32);
        for (i, (host, guest)) in socket_fwds.iter().enumerate() {
            sf_builder.reborrow().get(i as u32).set_host(host);
            sf_builder.reborrow().get(i as u32).set_guest(guest);
        }

        let response = req.send().promise.await?;
        let proc = response.get()?.get_proc()?;

        Ok(Process::new(proc))
    }

    /// Attach a new process to the running container.
    /// `cmd` and `args` are the fully-constructed invocation (e.g. `crun exec …`).
    pub async fn exec(
        &self,
        stdin: stdin::Client,
        pty_size: Option<(u16, u16)>,
        cmd: &str,
        args: &[String],
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
        let response = req.send().promise.await?;
        Ok(Process::new(response.get()?.get_proc()?))
    }

    /// Request the supervisor to sync filesystems before the VM is destroyed.
    pub async fn shutdown(&self) {
        let req = self.supervisor.shutdown_request();
        if let Err(e) = req.send().promise.await {
            tracing::debug!("shutdown RPC: {e}");
        }
    }
}
