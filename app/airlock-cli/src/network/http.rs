//! HTTP request interception via hyper.
//!
//! When the first bytes from the container look like HTTP, we hand off
//! to hyper's auto-detecting HTTP server (h1/h2) and h1/h2 client.
//! For each request, Lua scripts run and the (possibly modified) request
//! is forwarded via hyper client. Bodies are streamed, not buffered.

pub mod body;
mod executor;
pub mod middleware;
mod senders;

use std::cell::RefCell;
use std::rc::Rc;

use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::{debug, trace};

use crate::network::http::executor::LocalExecutor;
use crate::network::http::senders::{H1Sender, H2Sender, RequestSender};
use crate::network::target::ResolvedTarget;
use crate::network::{DenyReporter, io};

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

type ResponseBody = Either<Incoming, Full<Bytes>>;

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
                emit_request_event(&events, &req, &target_host, target_port, false);
                deny_reporter.report();
                let body: ResponseBody =
                    Either::Right(Full::new(Bytes::from("denied by network policy\n")));
                Ok::<_, hyper::Error>(Response::builder().status(403).body(body).unwrap())
            }
        });
        return hyper_util::server::conn::auto::Builder::new(LocalExecutor)
            .serve_connection(client_io, service)
            .await
            .map_err(|e| anyhow::anyhow!("http deny: {e}"));
    }

    let server_io = hyper_util::rt::TokioIo::new(tokio::io::join(server.read, server.write));
    debug!("http proxy: server h2 = {}", server.h2);

    let sender: Rc<dyn RequestSender> = if server.h2 {
        let (sender, conn): (hyper::client::conn::http2::SendRequest<ResponseBody>, _) =
            hyper::client::conn::http2::handshake(LocalExecutor, server_io).await?;
        tokio::task::spawn_local(conn);
        debug!("h2 client handshake complete");
        Rc::new(H2Sender(sender))
    } else {
        let (sender, conn): (hyper::client::conn::http1::SendRequest<ResponseBody>, _) =
            hyper::client::conn::http1::handshake(server_io).await?;
        tokio::task::spawn_local(conn);
        debug!("h1 client handshake complete");
        Rc::new(H1Sender(RefCell::new(sender)))
    };

    let middleware = target.middleware;
    let target_host = target.host.clone();
    let target_port = target.port;
    let allowed = target.allowed;
    let service = service_fn(move |req: Request<Incoming>| {
        let sender = sender.clone();
        let middleware = middleware.clone();
        let events = events.clone();
        let target_host = target_host.clone();
        let deny_reporter = deny_reporter.clone();
        async move {
            emit_request_event(&events, &req, &target_host, target_port, allowed);
            let result = middleware::run(req, &middleware, deny_reporter, move |req| {
                let sender = sender.clone();
                async move { sender.send(req).await.map_err(|e| anyhow::anyhow!("{e}")) }
            })
            .await;

            match result {
                Ok(resp) => Ok::<_, hyper::Error>(resp),
                Err(e) => {
                    debug!("middleware error: {e}");
                    Ok(Response::builder()
                        .status(502)
                        .body(Either::Right(Full::new(Bytes::from(format!("{e}\n")))))
                        .unwrap())
                }
            }
        }
    });

    hyper_util::server::conn::auto::Builder::new(LocalExecutor)
        .serve_connection(client_io, service)
        .await
        .map_err(|e| anyhow::anyhow!("http proxy: {e}"))
}

/// Broadcast a `NetworkEvent::Request` describing this HTTP request. Silently
/// drops the event when there are no subscribers — and short-circuits *before*
/// cloning any request fields in that common case (non-monitor runs).
fn emit_request_event(
    events: &tokio::sync::broadcast::Sender<airlock_monitor::NetworkEvent>,
    req: &Request<Incoming>,
    target_host: &str,
    target_port: u16,
    allowed: bool,
) {
    if events.receiver_count() == 0 {
        return;
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
    let info = airlock_monitor::RequestInfo {
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
}

/// Check if a line matches an HTTP request line or h2 connection preface.
fn is_http_request_line(line: &[u8]) -> bool {
    use std::sync::LazyLock;

    use regex::bytes::Regex;

    static H2_PREFACE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^PRI \* HTTP/2\.0$").unwrap());
    static H1_REQUEST: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Z]+ \S+ HTTP/\S+$").unwrap());

    H2_PREFACE.is_match(line) || H1_REQUEST.is_match(line)
}
