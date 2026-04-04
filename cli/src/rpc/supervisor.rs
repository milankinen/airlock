use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};

use ezpez_protocol::supervisor_capnp::*;

use crate::cli::CliArgs;
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

        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(vsock_fd.into_raw_fd()) };
        std_stream.set_nonblocking(true)?;
        let stream = tokio::net::TcpStream::from_std(std_stream)?;
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
    ) -> anyhow::Result<Process> {
        let log_sink: log_sink::Client = capnp_rpc::new_client(LogSinkImpl);

        let ca_cert = std::fs::read(&project.ca_cert)?;
        let ca_key = std::fs::read(&project.ca_key)?;

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
        req.get().set_ca_cert(&ca_cert);
        req.get().set_ca_key(&ca_key);
        req.get().set_logs(log_sink);
        req.get().set_log_filter(args.log_filter());

        let response = req.send().promise.await?;
        let proc = response.get()?.get_proc()?;

        Ok(Process::new(proc))
    }
}
