//! `NetworkProxy` RPC server implementation — the main entry point for all
//! outbound connections from the guest VM.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use ezpez_protocol::supervisor_capnp::{connect_target, network_proxy, tcp_sink};
use tokio::sync::mpsc;
use tracing::debug;

use super::target::ResolvedTarget;
use super::{Network, http, io, tcp, tls};

impl network_proxy::Server for Network {
    async fn connect(
        self: Rc<Self>,
        params: network_proxy::ConnectParams,
        mut results: network_proxy::ConnectResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let target = params.get_target()?;
        let client_sink = params.get_client()?;

        match target.which()? {
            connect_target::Tcp(tcp) => {
                let tcp = tcp?;
                let host = tcp.get_host()?.to_str()?.to_string();
                let port = tcp.get_port();

                let Some(net_target) = self.resolve_target(&host, port) else {
                    debug!("denied: {host}:{port} (no matching rule)");
                    results
                        .get()
                        .init_result()
                        .set_denied("no matching network rule");
                    return Ok(());
                };

                debug!("connect {host}:{port}");
                let sink = spawn_tcp_connection(
                    net_target,
                    client_sink,
                    self.tls_client.clone(),
                    self.interceptor.clone(),
                );
                results.get().init_result().set_server(sink);
            }
            connect_target::Socket(guest_path) => {
                let guest_path = guest_path?.to_str()?.to_string();
                let Some(host_path) = self.socket_map.get(&guest_path) else {
                    debug!("denied: socket {guest_path} (no matching rule)");
                    results
                        .get()
                        .init_result()
                        .set_denied("no matching socket rule");
                    return Ok(());
                };
                let host_path = host_path.to_string_lossy().into_owned();
                debug!("connect socket: {guest_path} → {host_path}");
                let sink = spawn_socket_connection(&host_path, client_sink);
                results.get().init_result().set_server(sink);
            }
        }
        Ok(())
    }
}

/// Spawn a background task for a TCP connection: detect TLS, optionally
/// intercept, apply middleware, and relay bytes bidirectionally.
fn spawn_tcp_connection(
    target: ResolvedTarget,
    client_sink: tcp_sink::Client,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
) -> tcp_sink::Client {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: io::RelayError = Rc::new(RefCell::new(None));
    let task_error = error.clone();

    tokio::task::spawn_local(async move {
        let addr = format!("{}:{}", target.host, target.port);
        let result = Box::pin(handle_connection(
            target,
            rx,
            client_sink,
            &tls_client,
            &interceptor,
        ))
        .await;

        if let Err(e) = result {
            debug!("connection {addr} error: {e}");
            *task_error.borrow_mut() = Some(format!("{e}"));
        }
    });

    capnp_rpc::new_client(io::ChannelSink::new(tx, error))
}

/// Spawn a background task for a Unix socket connection: connect to the
/// host-side socket and relay bytes bidirectionally.
fn spawn_socket_connection(path: &str, client_sink: tcp_sink::Client) -> tcp_sink::Client {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: io::RelayError = Rc::new(RefCell::new(None));
    let task_error = error.clone();

    let path = path.to_string();
    tokio::task::spawn_local(async move {
        let result: anyhow::Result<()> = async {
            let rpc_io = io::RpcTransport::new(Bytes::new(), rx, client_sink);
            let (cr, cw) = tokio::io::split(rpc_io);
            let container = io::Transport {
                read: Box::new(cr),
                write: Box::new(cw),
                h2: false,
            };

            let socket = tokio::time::timeout(
                crate::constants::SOCKET_CONNECT_TIMEOUT,
                tokio::net::UnixStream::connect(&path),
            )
            .await
            .map_err(|_| anyhow::anyhow!("socket connect timed out: {path}"))??;
            let (sr, sw) = socket.into_split();
            let server = io::Transport {
                read: Box::new(sr),
                write: Box::new(sw),
                h2: false,
            };

            Box::pin(tcp::relay(container, server)).await;
            Ok(())
        }
        .await;
        if let Err(e) = result {
            debug!("socket connection {path} error: {e}");
            *task_error.borrow_mut() = Some(format!("{e}"));
        }
    });

    capnp_rpc::new_client(io::ChannelSink::new(tx, error))
}

/// Main connection handler: detect TLS, decide on passthrough vs. intercept,
/// detect HTTP, and route to the appropriate relay.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    target: ResolvedTarget,
    mut rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    tls_client: &Arc<rustls::ClientConfig>,
    interceptor: &tls::TlsInterceptor,
) -> anyhow::Result<()> {
    let (is_tls, first) = tls::detect(&mut rx).await;
    let addr = format!("{}:{}", target.host, target.port);

    // Passthrough: raw relay, container↔server TLS end-to-end
    if target.is_passthrough() {
        debug!("passthrough: {addr}");
        let (container, server) = tcp::establish(&addr, first, rx, client_sink).await?;
        Box::pin(tcp::relay(container, server)).await;
        return Ok(());
    }

    // Establish connection pair
    let (container, server) = if is_tls {
        tls::establish(
            &target.host,
            target.port,
            first,
            rx,
            client_sink,
            interceptor,
            tls_client,
        )
        .await?
    } else {
        tcp::establish(&addr, first, rx, client_sink).await?
    };

    // Detect HTTP
    let (container, is_http) = detect_http(container).await;
    if is_http {
        Box::pin(http::relay(container, server, target)).await?;
    } else if target.http_only {
        anyhow::bail!("non-HTTP traffic rejected for {addr} (http-only target)");
    } else {
        Box::pin(tcp::relay(container, server)).await;
    }
    Ok(())
}

/// Peek at the container stream to detect HTTP.
async fn detect_http(mut container: io::Transport) -> (io::Transport, bool) {
    match http::detect(&mut container.read).await {
        Ok(prefix) => {
            container.read = Box::new(io::PrefixedRead::new(prefix, container.read));
            (container, true)
        }
        Err(buffered) => {
            container.read = Box::new(io::PrefixedRead::new(buffered, container.read));
            (container, false)
        }
    }
}
