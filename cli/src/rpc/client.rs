use crate::error::CliError;
use crate::rpc::process::Process;
use ezpez_protocol::supervisor_capnp::*;
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use ezpez_protocol::streams::OutputStream;

pub struct Client {
    supervisor: supervisor::Client,
}

impl Client {
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

    pub async fn exec(
        &self,
        stdin: impl Into<OutputStream>,
        rows: u16,
        cols: u16,
    ) -> Result<Process, CliError> {
        let mut req = self.supervisor.exec_request();
        req.get().set_stdin(stdin.into().into());
        let mut size = req.get().init_pty().init_size();
        size.set_rows(rows);
        size.set_cols(cols);

        let response = req.send().promise.await?;
        let proc = response.get()?.get_proc()?;

        Ok(Process::new(proc))
    }
}
