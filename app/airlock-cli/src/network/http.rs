//! HTTP request support.
//!
//! Detects HTTP traffic and relays requests from the sandbox to the upstream
//! server. The configured HTTP middlewares run for each request and response.
//! Also handles:
//!  * HTTP 1.1/2 conversion when the sandbox and the server use different
//!    versions
//!  * HTTP 1.1 upgrades, for example websockets
//!  * secret injection into requests
//!
//! Expects plaintext (TLS decrypted) traffic from both sides.

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
use http_body_util::combinators::UnsyncBoxBody;
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

/// Read the first bytes of a stream to detect HTTP.
///
/// Reads up to 4KB or until the first `\r\n`. Then checks if the line
/// matches `METHOD path HTTP/x.y\r\n` or the HTTP/2 preface.
/// Returns:
///   `Ok(buf)` if the stream is HTTP, `Err(buf)` if not. `buf` contains the
///   bytes that were read.
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

/// Request and response body on the send path of the relay. The body
/// streams from the peer, or the proxy makes it.
pub type ResponseBody = Either<StreamBody, Full<Bytes>>;

/// Streamed body: the body of the peer, or a peer body that an interceptor
/// wraps (the token scan of [`crate::services::scan`]).
pub type StreamBody = Either<Incoming, UnsyncBoxBody<Bytes, BoxError>>;

/// Error of a wrapped body.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Convert a body that streams from the peer into a [`ResponseBody`].
pub fn streamed(body: Incoming) -> ResponseBody {
    Either::Left(Either::Left(body))
}

/// hyper IO on a boxed read/write pair. Used for the guest side and the
/// upstream side.
type HyperIo = TokioIo<tokio::io::Join<io::BoxRead, io::BoxWrite>>;
type H1UpstreamConn = hyper::client::conn::http1::Connection<HyperIo, ResponseBody>;

/// Output of the upstream connection task. For an h1 upstream, it is the h1
/// connection object, so the relay can take it apart after an upgrade.
/// For h2, it is `None`.
type UpstreamDone = Option<H1UpstreamConn>;

/// Relay HTTP requests from the container to the server, with middleware.
/// Bodies stream and are not buffered.
/// Args:
///  - `container`: Plaintext container transport
///  - `server`: Plaintext server transport. [`io::Transport::null`] if the
///    target is denied.
///  - `target`: Resolved target with the decision, middleware, secrets and
///    interceptor
///  - `events`: Sender for monitor events
///  - `deny_reporter`: Notifier for denied requests.
///
/// Returns:
///   Ok when the connection ends, or error if the HTTP connection fails.
pub async fn relay(
    container: io::Transport,
    server: io::Transport,
    target: ResolvedTarget,
    events: tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    deny_reporter: Rc<DenyReporter>,
) -> anyhow::Result<()> {
    let client_io = hyper_util::rt::TokioIo::new(tokio::io::join(container.read, container.write));

    // For a denied target, still run a hyper server on the container side.
    // Then the request headers are parsed and show in the Requests sub-tab.
    // Each request gets a 403, and the server transport is not used.
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
    // Where the guest connected, as the interceptor sees it.
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
            // The monitor sees the request as the guest sent it (with the
            // surrogates). The event goes out before any secret is unmasked.
            let id = emit_request_event(&events, &req, &target_host, target_port, allowed);
            let wants_upgrade = upgradable && Upgrade::wants(&req);
            if wants_upgrade {
                upgrade.requested();
            }
            let method = req.method().clone();
            let connect_host: std::rc::Rc<str> = std::rc::Rc::from(target_host.as_str());
            // The innermost step: the interceptor that owns the host, then
            // the upstream. The interceptor runs after middleware, so
            // scripts and the monitor see only its surrogates.
            let send = {
                let (upgrade, method) = (upgrade.clone(), method.clone());
                let injected = secrets.clone();
                move |mut req: Request<ResponseBody>| async move {
                    inject::request_identity(req.headers_mut(), &injected);
                    let upstream: Next = Box::new(move |req| {
                        Box::pin(async move {
                            let resp =
                                sender.send(req).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                            Ok(resp.map(streamed))
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
            // Unmask before middleware, so scripts see the real request.
            // Mask again after middleware, so nothing that a script adds can
            // send the real value back into the guest. The body is masked
            // as it streams.
            let result = match inject::unmask_request(req.headers_mut(), &secrets) {
                Err(e) => Err(e),
                Ok(()) => middleware::run(req, &middleware, deny_reporter, connect_host, send)
                    .await
                    .and_then(|mut resp| {
                        inject::mask_response(resp.headers_mut(), &secrets)
                            .map(|()| inject::mask_body(resp, &secrets))
                    }),
            };

            let mut resp = match result {
                Ok(resp) => resp,
                Err(e) => {
                    // The request was unmasked before middleware ran. Thus a
                    // script error that quotes a header may contain the real
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
    // hyper ends a connection that had an upgrade request with
    // `Dispatched::Upgrade`, also if the protocol did not switch. It leaves
    // the socket open, with the bytes that it already read after the last
    // message.
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
            // The reply had `Connection: close`. Close the connection.
            let _ = guest.write.shutdown().await;
        }
    }
    Ok(())
}

/// Do the hyper client handshake on the upstream transport and run the
/// connection on its own task.
/// Args:
///  - `server_io`: Upstream transport
///  - `h2`: True if the upstream uses HTTP/2.
///
/// Returns:
///   Request sender and the connection task. The h1 task returns the
///   connection object when it ends. After an upgrade, the socket is still
///   open and the relay needs it.
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
/// Args:
///  - `guest`: Guest connection future
///  - `upstream`: Upstream connection task
///  - `upgrade`: Upgrade state of the connection
///  - `shutdown`: Function that starts a graceful shutdown of `guest`.
///
/// Returns:
///   The output of the upstream task, if the upstream is known to be done:
///   it ended first, or the guest ended on a protocol switch (the upstream
///   stops on the same reply). `None` if not.
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
            // When the upstream closes, do a graceful shutdown of the guest
            // connection. Then the guest sees a clean close and connects
            // again, and gets no 502s from an old sender.
            // Skip the shutdown while an upgrade is in progress. The
            // upstream h1 connection ends when it parses the 101. A shutdown then makes
            // hyper change `Connection: upgrade` of the 101 into
            // `Connection: close`. The guest connection then ends by itself
            // (see [`Upgrade::reply`]).
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

/// Send a `NetworkEvent::Request` for this HTTP request to the subscribers.
/// Returns:
///   The id of the request, to pair it with a later [`emit_response_event`].
///   `None` if no event was sent (no subscribers).
fn emit_request_event(
    events: &tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    req: &Request<Incoming>,
    target_host: &str,
    target_port: u16,
    allowed: bool,
) -> Option<u64> {
    // Usually there are no subscribers (runs without the monitor). Return
    // before the request fields are cloned in that case.
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

/// Send the response event for an earlier [`emit_request_event`].
/// A `None` id means that no request event was sent (no subscribers). Then
/// this function sends nothing.
///
/// Middleware runs after the request event, and only for requests that the
/// event shows as allowed. A 403 with the [`middleware::Denied`] tag comes
/// from `req:deny()` in a script. The response event then sets `denied`,
/// which overrides the decision of the request event.
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

/// Get a new monotonic request id. The Monitor tab uses the ids only to
/// pair each response with its request.
fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Return true if a line is an HTTP request line or the h2 connection
/// preface.
fn is_http_request_line(line: &[u8]) -> bool {
    use std::sync::LazyLock;

    use regex::bytes::Regex;

    static H1_REQUEST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Z]+ \S+ HTTP/\S+$").unwrap());

    is_h2_preface(line) || H1_REQUEST.is_match(line)
}

/// Return true if the sniffed first line is the HTTP/2 connection preface.
/// This means that the guest uses h2 (by ALPN or prior knowledge), not h1.
pub fn is_h2_preface(line: &[u8]) -> bool {
    line.starts_with(b"PRI * HTTP/2.0")
}
