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
            "/mnt/bundle".to_string(),
            "ezpez0".to_string(),
        ],
    )
}
use crate::network::Network;
use crate::project::Project;
use crate::rpc::logging::LogSinkImpl;
use crate::rpc::process::Process;
use crate::rpc::stdin::Stdin;

pub struct Supervisor {
    supervisor: supervisor::Client,
}

impl Supervisor {
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

    pub async fn start(
        &self,
        args: &CliArgs,
        project: &Project,
        stdin: Stdin,
        network: Network,
        cache_dirs: &[String],
    ) -> anyhow::Result<Process> {
        let log_sink: log_sink::Client = capnp_rpc::new_client(LogSinkImpl);

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
        let tls_passthrough = &project.config.network.tls_passthrough;
        let mut pt_builder = req.get().init_tls_passthrough(tls_passthrough.len() as u32);
        for (i, host) in tls_passthrough.iter().enumerate() {
            pt_builder.set(i as u32, host);
        }

        // Cache volume subdirs to create on /mnt/cache
        let mut cd_builder = req.get().init_cache_dirs(cache_dirs.len() as u32);
        for (i, dir) in cache_dirs.iter().enumerate() {
            cd_builder.set(i as u32, dir);
        }

        let response = req.send().promise.await?;
        let proc = response.get()?.get_proc()?;

        Ok(Process::new(proc))
    }
}
