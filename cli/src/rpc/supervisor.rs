use crate::error::CliError;
use crate::rpc::network::NetworkProxyImpl;
use crate::rpc::logging::LogSinkImpl;
use crate::rpc::process::Process;
use ezpez_protocol::supervisor_capnp::*;
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::Path;

pub struct Supervisor {
    supervisor: supervisor::Client,
}

impl Supervisor {
    pub fn connect(vsock_fd: OwnedFd) -> Result<Self, CliError> {
        use futures::AsyncReadExt;

        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(vsock_fd.into_raw_fd()) };
        std_stream.set_nonblocking(true)?;
        let stream = tokio::net::TcpStream::from_std(std_stream)?;
        let (reader, writer) =
            tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

        let network = capnp_rpc::twoparty::VatNetwork::new(
            reader,
            writer,
            capnp_rpc::rpc_twoparty_capnp::Side::Client,
            Default::default(),
        );

        let mut rpc = capnp_rpc::RpcSystem::new(Box::new(network), None);
        let client: supervisor::Client =
            rpc.bootstrap(capnp_rpc::rpc_twoparty_capnp::Side::Server);

        tokio::task::spawn_local(rpc);

        Ok(Self { supervisor: client })
    }

    pub async fn start(
        &self,
        stdin: stdin::Client,
        rows: u16,
        cols: u16,
        ca_cert_path: &Path,
        ca_key_path: &Path,
    ) -> Result<Process, CliError> {
        let network_proxy: network_proxy::Client =
            capnp_rpc::new_client(NetworkProxyImpl);
        let log_sink: log_sink::Client =
            capnp_rpc::new_client(LogSinkImpl);

        let ca_cert = std::fs::read(ca_cert_path)?;
        let ca_key = std::fs::read(ca_key_path)?;

        let mut req = self.supervisor.start_request();
        req.get().set_stdin(stdin);
        let mut size = req.get().init_pty().init_size();
        size.set_rows(rows);
        size.set_cols(cols);
        req.get().set_network(network_proxy);
        req.get().set_ca_cert(&ca_cert);
        req.get().set_ca_key(&ca_key);
        req.get().set_logs(log_sink);

        let response = req.send().promise.await?;
        let proc = response.get()?.get_proc()?;

        Ok(Process::new(proc))
    }
}
