//! HTTP request interception via hyper.
//!
//! When the first bytes from the container look like HTTP, we hand off
//! to a hyper HTTP server (h1 or h2, by the sniffed preface) and h1/h2
//! client. HTTP/1.1 upgrades are handled in [`upgrade`].
//! For each request, Lua scripts run and the (possibly modified) request
//! is forwarded via hyper client. Bodies are streamed, not buffered.

pub mod body;
mod executor;
pub mod inject;
pub mod middleware;
mod senders;
mod upgrade;

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;

use anyhow::Context as _;
use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tracing::{debug, trace};

use crate::network::http::executor::LocalExecutor;
use crate::network::http::senders::{H1Sender, H2Sender, RequestSender};
use crate::network::http::upgrade::Upgrade;
use crate::network::interceptor::Next;
use crate::network::target::{Endpoint, ResolvedTarget};
use crate::network::{DenyReporter, io, tcp};

const MAX_DETECT_SIZE: usize = 4096;

/// Peek at the first bytes to detect HTTP.
///
/// Reads up to 4KB or until the first `\r\n`, then checks if the line
/// matches `METHOD path HTTP/x.y\r\n`. Returns `Ok(buf)` if HTTP,
/// `Err(buf)` if not.
pub async fn detect(reader: &mut (impl AsyncRead + Unpin)) -> Result<Bytes, Bytes> {
    let mut buf = bytes::BytesMut::zeroed(MAX_DETECT_SIZE);
    let mut len = 0;
    loop {
        let n = match reader.read(&mut buf[len..]).await {
            Ok(0) | Err(_) => {
                trace!("stream closed before HTTP detection ({len} bytes)");
                buf.truncate(len);
                return Err(buf.freeze());
            }
            Ok(n) => n,
        };
        len += n;

        if let Some(pos) = buf[..len].windows(2).position(|w| w == b"\r\n") {
            buf.truncate(len);
            return if is_http_request_line(&buf[..pos]) {
                debug!("detected HTTP request line");
                Ok(buf.freeze())
            } else {
                trace!(
                    "first line is not HTTP: {:?}",
                    String::from_utf8_lossy(&buf[..pos.min(80)])
                );
                Err(buf.freeze())
            };
        }

        if len >= MAX_DETECT_SIZE {
            trace!("no linebreak in first {MAX_DETECT_SIZE}B, not HTTP");
            buf.truncate(len);
            return Err(buf.freeze());
        }
    }
}

/// Request and response bodies on the relay's send path: streamed from
/// the peer, or built by the proxy.
pub type ResponseBody = Either<Incoming, Full<Bytes>>;

/// hyper IO over a boxed read/write pair — both guest and upstream sides.
type HyperIo = TokioIo<tokio::io::Join<io::BoxRead, io::BoxWrite>>;
type H1UpstreamConn = hyper::client::conn::http1::Connection<HyperIo, ResponseBody>;

/// The upstream connection task's output: the h1 connection object when
/// the upstream speaks h1 (so it can be taken apart after an upgrade),
/// `None` for h2.
type UpstreamDone = Option<H1UpstreamConn>;

/// Run hyper HTTP proxy with middleware interception.
///
/// When `target.allowed` is false, `server` is a [`io::Transport::null`]
/// black hole. We still run a hyper server against the container so the
/// request headers are parsed and surfaced in the Requests sub-tab, but we
/// short-circuit with a 403 before touching the server transport.
pub async fn relay(
    container: io::Transport,
    server: io::Transport,
    target: ResolvedTarget,
    events: tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    deny_reporter: Rc<DenyReporter>,
) -> anyhow::Result<()> {
    let client_io = hyper_util::rt::TokioIo::new(tokio::io::join(container.read, container.write));

    if !target.allowed {
        let target_host = target.host.clone();
        let target_port = target.port;
        let deny_reporter = deny_reporter.clone();
        let service = service_fn(move |req: Request<Incoming>| {
            let events = events.clone();
            let target_host = target_host.clone();
            let deny_reporter = deny_reporter.clone();
            async move {
                let id = emit_request_event(&events, &req, &target_host, target_port, false);
                deny_reporter.report();
                let body: ResponseBody =
                    Either::Right(Full::new(Bytes::from("denied by network policy\n")));
                let resp = Response::builder().status(403).body(body).unwrap();
                emit_response_event(&events, id, &resp);
                Ok::<_, hyper::Error>(resp)
            }
        });
        return hyper_util::server::conn::auto::Builder::new(LocalExecutor)
            .serve_connection(client_io, service)
            .await
            .map_err(|e| anyhow::anyhow!("http deny: {e}"));
    }

    let server_io = hyper_util::rt::TokioIo::new(tokio::io::join(server.read, server.write));
    debug!("http proxy: server h2 = {}", server.h2);
    let (sender, upstream) = connect_upstream(server_io, server.h2).await?;

    // Upgrades exist only in HTTP/1.1, and need it on both hops.
    let upgradable = !container.h2 && !server.h2;
    let upgrade = Rc::new(Upgrade::default());

    let middleware = target.middleware;
    let secrets = target.secrets;
    let interceptor = target.interceptor;
    // The interceptor's view of where the guest connected.
    let endpoint = Rc::new(Endpoint::new(&target.host, target.port));
    let target_host = target.host.clone();
    let target_port = target.port;
    let allowed = target.allowed;
    let upgrade_shared = upgrade.clone();
    let service = service_fn(move |mut req: Request<Incoming>| {
        let sender = sender.clone();
        let middleware = middleware.clone();
        let secrets = secrets.clone();
        let interceptor = interceptor.clone();
        let endpoint = endpoint.clone();
        let events = events.clone();
        let target_host = target_host.clone();
        let deny_reporter = deny_reporter.clone();
        let upgrade = upgrade_shared.clone();
        async move {
            // The monitor sees the request as the guest sent it (surrogates
            // intact): the event is emitted before any secret is unmasked.
            let id = emit_request_event(&events, &req, &target_host, target_port, allowed);
            let wants_upgrade = upgradable && Upgrade::wants(&req);
            if wants_upgrade {
                upgrade.requested();
            }
            let method = req.method().clone();
            let connect_host: std::rc::Rc<str> = std::rc::Rc::from(target_host.as_str());
            // The innermost step: an interceptor that owns the host (after
            // middleware, so scripts and the monitor see its surrogates
            // only), then the upstream.
            let send = {
                let (upgrade, method) = (upgrade.clone(), method.clone());
                let injected = secrets.clone();
                move |req| async move {
                    let upstream: Next = Box::new(move |req| {
                        Box::pin(async move {
                            let resp =
                                sender.send(req).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                            Ok(resp.map(Either::Left))
                        })
                    });
                    let resp = match interceptor {
                        Some(interceptor) => {
                            trace!("interceptor {}: {}", interceptor.name(), endpoint.host());
                            interceptor
                                .send(&endpoint, req, &injected, upstream)
                                .await?
                        }
                        None => upstream(req).await?,
                    };
                    if wants_upgrade {
                        upgrade.upstream_replied(&method, &resp);
                    }
                    Ok(resp)
                }
            };
            // Unmask before middleware so scripts observe the real request;
            // re-mask after middleware so nothing a script adds can carry the
            // real value back into the guest.
            let result = match inject::unmask_request(req.headers_mut(), &secrets) {
                Err(e) => Err(e),
                Ok(()) => middleware::run(req, &middleware, deny_reporter, connect_host, send)
                    .await
                    .and_then(|mut resp| {
                        inject::mask_response(resp.headers_mut(), &secrets).map(|()| resp)
                    }),
            };

            let mut resp = match result {
                Ok(resp) => resp,
                Err(e) => {
                    // The request was unmasked before middleware ran, so a
                    // script error that quotes a header may carry the real
                    // secret. Mask the text before it is logged or sent.
                    let msg = inject::mask_text(&e.to_string(), &secrets);
                    debug!("middleware error: {msg}");
                    text_response(StatusCode::BAD_GATEWAY, &format!("{msg}\n"))
                }
            };
            if wants_upgrade {
                upgrade.reply(&method, &mut resp);
            }
            emit_response_event(&events, id, &resp);
            Ok::<_, hyper::Error>(resp)
        }
    });

    if container.h2 {
        let guest = hyper::server::conn::http2::Builder::new(LocalExecutor)
            .serve_connection(client_io, service);
        let mut guest = std::pin::pin!(guest);
        drive_guest(guest.as_mut(), upstream, &upgrade, |c| {
            c.graceful_shutdown();
        })
        .await?;
        return Ok(());
    }

    let mut guest = hyper::server::conn::http1::Builder::new().serve_connection(client_io, service);
    let upstream = drive_guest(Pin::new(&mut guest), upstream, &upgrade, |c| {
        c.graceful_shutdown();
    })
    .await?;
    if !upgrade.in_flight() {
        return Ok(());
    }
    // hyper ends a connection that carried an upgrade request with
    // `Dispatched::Upgrade`, switch or not, and leaves its socket open for
    // us — plus whatever bytes it already read past the last message.
    let guest = guest.into_parts();
    let guest_buffered = guest.read_buf.len();
    let mut guest = upgrade::transport(guest.io, guest.read_buf);
    match upstream {
        Some(Some(upstream)) if upgrade.switched() => {
            let upstream = upstream.into_parts();
            debug!(
                "http upgrade: relaying raw bytes (guest buffered {guest_buffered}B, upstream buffered {}B)",
                upstream.read_buf.len()
            );
            tcp::relay(guest, upgrade::transport(upstream.io, upstream.read_buf)).await;
        }
        _ => {
            // The reply carried `Connection: close`; make the close real.
            let _ = guest.write.shutdown().await;
        }
    }
    Ok(())
}

/// Handshake a hyper client on the upstream transport and drive it on its
/// own task. The h1 task hands the connection object back when it ends:
/// after an upgrade the socket is still open and the relay needs it.
async fn connect_upstream(
    server_io: HyperIo,
    h2: bool,
) -> anyhow::Result<(Rc<dyn RequestSender>, JoinHandle<UpstreamDone>)> {
    if h2 {
        let (sender, conn): (hyper::client::conn::http2::SendRequest<ResponseBody>, _) =
            hyper::client::conn::http2::handshake(LocalExecutor, server_io).await?;
        let task = tokio::task::spawn_local(async move {
            if let Err(e) = conn.await {
                debug!("upstream h2 connection: {e}");
            }
            None
        });
        debug!("h2 client handshake complete");
        return Ok((Rc::new(H2Sender(sender)), task));
    }
    let (sender, mut conn): (hyper::client::conn::http1::SendRequest<ResponseBody>, _) =
        hyper::client::conn::http1::handshake(server_io).await?;
    let task = tokio::task::spawn_local(async move {
        if let Err(e) = (&mut conn).await {
            debug!("upstream h1 connection: {e}");
        }
        Some(conn)
    });
    debug!("h1 client handshake complete");
    Ok((Rc::new(H1Sender(RefCell::new(sender))), task))
}

/// Serve the guest connection until it ends.
///
/// Mirrors an upstream close with a graceful shutdown, so the guest sees a
/// clean close and reconnects instead of getting 502s from a stale sender.
/// Not while an upgrade is in flight: the upstream h1 connection ends the
/// moment it parses the 101, and a shutdown then makes hyper rewrite the
/// 101's `Connection: upgrade` into `Connection: close`. The guest
/// connection ends on its own in that case (see [`Upgrade::reply`]).
///
/// Returns the upstream task's output once the upstream is known to be
/// done: it ended first, or the guest ended on a switch (the upstream
/// stops on the same reply).
async fn drive_guest<C>(
    mut guest: Pin<&mut C>,
    mut upstream: JoinHandle<UpstreamDone>,
    upgrade: &Upgrade,
    shutdown: fn(Pin<&mut C>),
) -> anyhow::Result<Option<UpstreamDone>>
where
    C: Future<Output = hyper::Result<()>>,
{
    let done = tokio::select! {
        result = guest.as_mut() => {
            result.context("http proxy")?;
            None
        }
        done = &mut upstream => {
            if !upgrade.in_flight() {
                debug!("upstream connection closed, shutting down guest connection");
                shutdown(guest.as_mut());
            }
            guest.await.context("http proxy shutdown")?;
            Some(done.context("upstream connection task")?)
        }
    };
    match done {
        None if upgrade.switched() => Ok(Some(upstream.await.context("upstream connection task")?)),
        done => Ok(done),
    }
}

fn text_response(status: StatusCode, body: &str) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(Either::Right(Full::new(Bytes::from(body.to_string()))))
        .unwrap()
}

/// Broadcast a `NetworkEvent::Request` describing this HTTP request. Silently
/// drops the event when there are no subscribers — and short-circuits *before*
/// cloning any request fields in that common case (non-monitor runs).
///
/// Returns the id assigned to the request, for pairing with a later
/// [`emit_response_event`]; `None` when nothing was emitted.
fn emit_request_event(
    events: &tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    req: &Request<Incoming>,
    target_host: &str,
    target_port: u16,
    allowed: bool,
) -> Option<u64> {
    if events.receiver_count() == 0 {
        return None;
    }
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect();
    let id = next_request_id();
    let info = airlock_monitor::RequestInfo {
        id,
        timestamp: std::time::SystemTime::now(),
        method,
        path,
        host: target_host.to_string(),
        port: target_port,
        allowed,
        headers,
    };
    let _ = events.send(airlock_monitor::NetworkEvent::Request(std::sync::Arc::new(
        info,
    )));
    Some(id)
}

/// Broadcast the response paired to a prior [`emit_request_event`]. A
/// `None` id means the request was never reported (no subscribers), so
/// there's nothing to pair with.
///
/// Middleware runs after the request event went out, and only for requests
/// that event reported allowed. A 403 tagged [`middleware::Denied`] is a
/// script's `req:deny()`: the response event sets `denied` to overturn the
/// request event's verdict.
fn emit_response_event<B>(
    events: &tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    id: Option<u64>,
    resp: &Response<B>,
) {
    let Some(id) = id else {
        return;
    };
    if events.receiver_count() == 0 {
        return;
    }
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("<binary>").to_string(),
            )
        })
        .collect();
    let info = airlock_monitor::ResponseInfo {
        id,
        status: resp.status().as_u16(),
        headers,
        denied: resp.extensions().get::<middleware::Denied>().is_some(),
    };
    let _ = events.send(airlock_monitor::NetworkEvent::Response(
        std::sync::Arc::new(info),
    ));
}

/// Monotonic request ids, used only to pair a response back to its
/// request in the Monitor tab.
fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Check if a line matches an HTTP request line or h2 connection preface.
fn is_http_request_line(line: &[u8]) -> bool {
    use std::sync::LazyLock;

    use regex::bytes::Regex;

    static H1_REQUEST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Z]+ \S+ HTTP/\S+$").unwrap());

    is_h2_preface(line) || H1_REQUEST.is_match(line)
}

/// True when the sniffed first line is the HTTP/2 connection preface, i.e.
/// the guest speaks h2 (by ALPN or prior knowledge) rather than h1.
pub fn is_h2_preface(line: &[u8]) -> bool {
    line.starts_with(b"PRI * HTTP/2.0")
}
