use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use ezpez_protocol::supervisor_capnp::*;
use futures::AsyncReadExt;
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;


struct SupervisorImpl;

impl supervisor::Server for SupervisorImpl {
    async fn ping(
        self: Rc<Self>,
        _params: supervisor::PingParams,
        mut results: supervisor::PingResults,
    ) -> Result<(), capnp::Error> {
        results.get().set_id(0);
        Ok(())
    }

    async fn exec(
        self: Rc<Self>,
        params: supervisor::ExecParams,
        mut results: supervisor::ExecResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let stdin = params.get_stdin()?;
        let pty_config = params.get_pty()?;

        let use_pty = match pty_config.which() {
            Ok(pty_config::Size(size)) => {
                let size = size?;
                Some((size.get_rows(), size.get_cols()))
            }
            _ => None,
        };

        let proc = super::process::spawn(stdin, use_pty)?;
        results.get().set_proc(proc);

        Ok(())
    }
}

pub async fn serve(conn_fd: OwnedFd) -> anyhow::Result<()> {
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd.into_raw_fd()) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );

    let client: supervisor::Client = capnp_rpc::new_client(SupervisorImpl);
    let rpc = RpcSystem::new(Box::new(network), Some(client.client));

    let local = tokio::task::LocalSet::new();
    local.run_until(rpc).await?;

    Ok(())
}
