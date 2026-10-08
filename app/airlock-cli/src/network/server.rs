//! Network proxy server for the guest.
//!
//! This is the entry point for all outgoing connections from the guest VM:
//! TCP connections and Unix socket connections.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use airlock_common::network_capnp::{connect_target, network_proxy, tcp_sink};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::debug;

use super::target::ResolvedTarget;
use super::{DenyReporter, Network, http, io, tcp, tls, traffic};

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

                // Deferred deny: accept the TCP connect of the guest also
                // when the policy denies it. Then an HTTP request on the
                // connection still gets to the relay layer, and the Requests
                // sub-tab can show its method, path and headers. The relay
                // then sends 403 and does not forward upstream. A denied
                // non-HTTP connection closes after detection.
                let net_target = self.resolve_target(&host, port);
                debug!(
                    "connect {host}:{port} ({})",
                    if net_target.allowed {
                        "allowed"
                    } else {
                        "denied"
                    }
                );
                let id = self.next_connection_id();
                self.emit_connect(id, &net_target.host, net_target.port, net_target.allowed);
                let sink = spawn_tcp_connection(
                    id,
                    net_target,
                    client_sink,
                    self.tls_client.clone(),
                    self.interceptor.clone(),
                    self.events.clone(),
                    self.deny_reporter.clone(),
                );
                results.get().init_result().set_server(sink);
            }
            connect_target::Socket(guest_path) => {
                let guest_path = guest_path?.to_str()?.to_string();

                if self.is_deny_always() {
                    debug!("denied: socket {guest_path} (denied by policy)");
                    let id = self.next_connection_id();
                    self.emit_connect(id, &guest_path, 0, false);
                    self.emit_disconnect(id);
                    self.deny_reporter.report();
                    results.get().init_result().set_denied("denied by policy");
                    return Ok(());
                }

                let Some(host_path) = self.socket_map.get(&guest_path) else {
                    debug!("denied: socket {guest_path} (no matching rule)");
                    let id = self.next_connection_id();
                    self.emit_connect(id, &guest_path, 0, false);
                    self.emit_disconnect(id);
                    self.deny_reporter.report();
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

/// Start a background task for a TCP connection. The task detects TLS,
/// intercepts if necessary, applies middleware, and relays bytes in both
/// directions.
/// Returns:
///   RPC sink that receives the container bytes of the connection.
fn spawn_tcp_connection(
    id: u64,
    target: ResolvedTarget,
    client_sink: tcp_sink::Client,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    events: tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    deny_reporter: Rc<DenyReporter>,
) -> tcp_sink::Client {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: io::RelayError = Rc::new(RefCell::new(None));
    let task_error = error.clone();

    tokio::task::spawn_local(async move {
        let addr = format!("{}:{}", target.host, target.port);
        // `None` on runs without the monitor. Then the relay transports have
        // no wrapper, and the hot path does no byte counting.
        let counter = traffic::TrafficCounter::new(id, &events);
        let result = Box::pin(handle_connection(
            target,
            rx,
            client_sink,
            &tls_client,
            &interceptor,
            events.clone(),
            deny_reporter,
            counter.as_ref(),
        ))
        .await;

        if let Err(e) = result {
            debug!("connection {addr} error: {e:#}");
            *task_error.borrow_mut() = Some(format!("{e}"));
        }

        // Send the final totals before the row becomes gray. If not, a
        // transfer shorter than the throttle window never sends a report.
        if let Some(counter) = counter.as_ref() {
            counter.flush();
        }

        // Send the matching `Disconnect`. The TUI then changes the row
        // indicator from green (open) to gray (closed) and records the
        // close time.
        if events.receiver_count() > 0 {
            let info = airlock_monitor::DisconnectInfo {
                id,
                timestamp: std::time::SystemTime::now(),
            };
            let _ = events.send(airlock_monitor::NetworkEvent::Disconnect(Arc::new(info)));
        }
    });

    capnp_rpc::new_client(io::ChannelSink::new(tx, error))
}

/// Start a background task for a Unix socket connection. The task connects
/// to the host socket at `path` and relays bytes in both directions.
/// Returns:
///   RPC sink that receives the container bytes of the connection.
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
            debug!("socket connection {path} error: {e:#}");
            *task_error.borrow_mut() = Some(format!("{e}"));
        }
    });

    capnp_rpc::new_client(io::ChannelSink::new(tx, error))
}

/// Handle one TCP connection. Detect TLS and intercept it (MITM), detect
/// HTTP, and send the connection to the correct relay.
// Each argument is a different collaborator from `spawn_tcp_connection`.
// A struct for them would only move the same list one level up.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    mut target: ResolvedTarget,
    mut rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    tls_client: &Arc<rustls::ClientConfig>,
    interceptor: &tls::TlsInterceptor,
    events: tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    deny_reporter: Rc<DenyReporter>,
    counter: Option<&Rc<traffic::TrafficCounter>>,
) -> anyhow::Result<()> {
    let addr = format!("{}:{}", target.host, target.port);

    // Passthrough targets skip all detection. The proxy connects to the real
    // server immediately and relays raw bytes. Non-HTTP protocols need this
    // when their first client bytes cannot be sniffed. For example, the
    // 8-byte `SSLRequest` of Postgres blocks the HTTP detector, which waits
    // for `\r\n`.
    if target.is_passthrough() {
        debug!("passthrough: {addr}");
        let container = tcp::container_transport(Bytes::new(), rx, client_sink);
        let container = traffic::count(container, counter);
        let server = tcp::connect_server(&target).await?;
        Box::pin(tcp::relay(container, server)).await;
        return Ok(());
    }

    // Accept TLS and detect HTTP also when the policy denies the target.
    // This is why the deny decision waits until this phase: a denied HTTP
    // request shows in the Requests sub-tab with all details, and does not
    // disappear behind an early TCP reset.
    let (is_tls, first) = tls::detect(&mut rx).await;

    // Make the container-side transport first. It is the same for allow and
    // deny.
    //
    // Count bytes on the raw RPC stream on both branches, so the Monitor tab
    // shows wire bytes. On the TLS branch, `accept_container` counts below
    // the TLS layer that it terminates. On the plain branch, the transport
    // *is* the raw stream. `detect_http` replays the sniffed prefix outside
    // the counter, so it does not count those bytes two times.
    let (container, alpn) = if is_tls {
        tls::accept_container(&target.host, first, rx, client_sink, interceptor, counter).await?
    } else {
        (
            traffic::count(tcp::container_transport(first, rx, client_sink), counter),
            None,
        )
    };

    // A network service handles only TLS connections. On plain HTTP, the
    // surrogates of the guest go out with no change.
    if !is_tls {
        target.interceptor = None;
    }
    let (container, is_http) = detect_http(container).await;

    // All fail-closed handling of an owned host (token swaps, backstops) is
    // in the interceptor call of the HTTP relay. Bytes that are not HTTP
    // never get there. If the proxy relayed them raw to the real upstream,
    // a guest could send anything past the service (also a real token that
    // it got in some other way). Thus refuse, and do not connect upstream.
    if target.interceptor.is_some() && !is_http {
        debug!("denied: {addr} (owned host sent non-HTTP bytes)");
        deny_reporter.report();
        return Ok(());
    }

    let server = match (target.allowed, is_tls) {
        (false, _) => io::Transport::null(),
        (true, true) => tls::connect_server(&target, alpn.as_deref(), tls_client).await?,
        (true, false) => tcp::connect_server(&target).await?,
    };
    if is_http {
        Box::pin(http::relay(
            container,
            server,
            target,
            events,
            deny_reporter,
        ))
        .await?;
    } else {
        if !target.allowed {
            deny_reporter.report();
        }
        Box::pin(tcp::relay(container, server)).await;
    }
    Ok(())
}

/// Read the start of the container stream to detect HTTP.
/// Returns:
///   The container transport, with the read bytes put back in front, and
///   true if the stream is HTTP.
async fn detect_http(mut container: io::Transport) -> (io::Transport, bool) {
    match http::detect(&mut container.read).await {
        Ok(prefix) => {
            // The protocol that the guest really uses sets the server-side
            // hyper mode. An h2 ALPN selection with no h2 preface is h1.
            container.h2 = http::is_h2_preface(&prefix);
            container.read = Box::new(io::PrefixedRead::new(prefix, container.read));
            (container, true)
        }
        Err(buffered) => {
            container.read = Box::new(io::PrefixedRead::new(buffered, container.read));
            (container, false)
        }
    }
}
