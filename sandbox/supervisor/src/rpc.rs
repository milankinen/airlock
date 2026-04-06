use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;

use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use ezpez_protocol::supervisor_capnp::*;
use futures::AsyncReadExt;

use crate::init::InitConfig;

pub struct HostConnection {
    pub proc: HostProcess,
    pub network: network_proxy::Client,
    pub cmd: String,
    pub args: Vec<String>,
    pub init_config: InitConfig,
}

pub struct HostProcess {
    pub stdin: stdin::Client,
    pub pty_size: Option<(u16, u16)>,
    /// Send Ok(process) on success, or Err(message) on init failure.
    /// Taken by the startup code — None after process is spawned.
    pub result: Option<tokio::sync::oneshot::Sender<Result<process::Client, String>>>,
}

pub async fn connect(
    conn_fd: OwnedFd,
) -> anyhow::Result<(log_sink::Client, String, HostConnection)> {
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd.into_raw_fd()) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );

    let (conn_tx, conn_rx) = tokio::sync::oneshot::channel::<ConnPayload>();

    let client: supervisor::Client =
        capnp_rpc::new_client(SupervisorImpl(std::cell::RefCell::new(Some(conn_tx))));
    let rpc = RpcSystem::new(Box::new(network), Some(client.client));

    tokio::task::spawn_local(rpc);

    conn_rx
        .await
        .map_err(|_| anyhow::anyhow!("host disconnected before start()"))
}

type ConnPayload = (log_sink::Client, String, HostConnection);

struct SupervisorImpl(std::cell::RefCell<Option<tokio::sync::oneshot::Sender<ConnPayload>>>);

impl supervisor::Server for SupervisorImpl {
    async fn start(
        self: Rc<Self>,
        params: supervisor::StartParams,
        mut results: supervisor::StartResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;

        let pty_size = match params.get_pty()?.which() {
            Ok(pty_config::Size(size)) => {
                let size = size?;
                Some((size.get_rows(), size.get_cols()))
            }
            _ => None,
        };

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        let log_sink = params.get_logs()?;
        let log_filter = params.get_log_filter()?.to_str()?.to_string();
        let conn = HostConnection {
            proc: HostProcess {
                stdin: params.get_stdin()?,
                pty_size,
                result: Some(result_tx),
            },
            network: params.get_network()?,
            cmd: params.get_cmd()?.to_str()?.to_string(),
            args: params
                .get_args()?
                .iter()
                .map(|a| a.map(|s| s.to_str().unwrap_or("").to_string()))
                .collect::<Result<Vec<_>, _>>()?,
            init_config: InitConfig {
                epoch: params.get_epoch(),
                host_ports: params.get_host_ports()?.iter().collect(),
            },
        };

        if let Some(tx) = self.0.borrow_mut().take() {
            let _ = tx.send((log_sink, log_filter, conn));
        }

        match result_rx.await {
            Ok(Ok(proc)) => {
                results.get().set_proc(proc);
                Ok(())
            }
            Ok(Err(msg)) => Err(capnp::Error::failed(msg)),
            Err(_) => Err(capnp::Error::failed("supervisor setup dropped".into())),
        }
    }

    async fn shutdown(
        self: Rc<Self>,
        _params: supervisor::ShutdownParams,
        _results: supervisor::ShutdownResults,
    ) -> Result<(), capnp::Error> {
        tracing::info!("shutdown: syncing filesystems");
        unsafe { libc::sync() };
        tracing::info!("shutdown: sync complete");
        Ok(())
    }
}
