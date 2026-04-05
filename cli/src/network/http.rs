//! HTTP request interception via hyper.
//!
//! When the first bytes from the container look like HTTP, we hand off
//! to hyper's auto-detecting HTTP server (h1/h2) and h1/h2 client.
//! For each request, Lua scripts run and the (possibly modified) request
//! is forwarded via hyper client. Bodies are streamed, not buffered.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::{debug, trace};

use super::scripting::{HttpRequestInfo, ScriptEngine, TcpConnect};
use crate::network::io;

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

        // Look for the first line ending
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

/// Run hyper HTTP proxy with auto h1/h2 detection. Bodies stream through.
#[allow(clippy::too_many_arguments)]
pub async fn relay(
    container: io::Transport,
    server: io::Transport,
    engine: &Rc<ScriptEngine>,
    connect: TcpConnect,
) -> anyhow::Result<()> {
    let client_io = hyper_util::rt::TokioIo::new(tokio::io::join(container.read, container.write));
    let server_io = hyper_util::rt::TokioIo::new(tokio::io::join(server.read, server.write));
    debug!("http proxy: server h2 = {}", server.h2);
    let sender: Rc<dyn RequestSender> = if server.h2 {
        let (sender, conn) = hyper::client::conn::http2::handshake(LocalExec, server_io).await?;
        tokio::task::spawn_local(conn);
        debug!("h2 client handshake complete");
        Rc::new(H2Sender(sender))
    } else {
        let (sender, conn) = hyper::client::conn::http1::handshake(server_io).await?;
        tokio::task::spawn_local(conn);
        debug!("h1 client handshake complete");
        Rc::new(H1Sender(RefCell::new(sender)))
    };

    let engine = engine.clone();
    let service = service_fn(move |req: Request<Incoming>| {
        let engine = engine.clone();
        let connect = connect.clone();
        let sender = sender.clone();
        async move { handle_request(req, &engine, &connect, sender, server.h2).await }
    });

    hyper_util::server::conn::auto::Builder::new(LocalExec)
        .serve_connection(client_io, service)
        .await
        .map_err(|e| anyhow::anyhow!("http proxy: {e}"))
}

/// Executor that spawns futures on the current LocalSet.
#[derive(Clone)]
struct LocalExec;

impl<F> hyper::rt::Executor<F> for LocalExec
where
    F: Future + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn_local(fut);
    }
}

/// Abstraction over h1/h2 client senders.
trait RequestSender {
    fn send(
        &self,
        req: Request<Incoming>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>>;
}

struct H1Sender(RefCell<hyper::client::conn::http1::SendRequest<Incoming>>);
impl RequestSender for H1Sender {
    fn send(
        &self,
        req: Request<Incoming>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        Box::pin(self.0.borrow_mut().send_request(req))
    }
}

/// h2 SendRequest is Clone and safe for concurrent use — clone per request.
struct H2Sender(hyper::client::conn::http2::SendRequest<Incoming>);
impl RequestSender for H2Sender {
    fn send(
        &self,
        req: Request<Incoming>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        let mut sender = self.0.clone();
        Box::pin(async move { sender.send_request(req).await })
    }
}

type ResponseBody = Either<Incoming, Full<Bytes>>;

fn safe_header_value<'a>(name: &str, value: &'a str) -> &'a str {
    const SENSITIVE: &[&str] = &[
        "authorization",
        "cookie",
        "set-cookie",
        "proxy-authorization",
    ];
    if SENSITIVE.iter().any(|&s| s.eq_ignore_ascii_case(name)) {
        "[redacted]"
    } else {
        value
    }
}

async fn handle_request(
    req: Request<Incoming>,
    engine: &ScriptEngine,
    connect: &TcpConnect,
    sender: Rc<dyn RequestSender>,
    h2: bool,
) -> Result<Response<ResponseBody>, hyper::Error> {
    let method = req.method().clone();
    let path = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    debug!("http: {method} {path} ({} headers)", headers.len());
    for (k, v) in &headers {
        trace!("  > {k}: {}", safe_header_value(k, v));
    }

    let mut http_req = HttpRequestInfo {
        connect: connect.clone(),
        method: method.to_string(),
        path,
        headers,
        allowed: true,
        denied: false,
    };

    if let Err(e) = engine.intercept_http_request(&mut http_req) {
        debug!("http denied: {method} {} ({e})", http_req.path);
        return Ok(Response::builder()
            .status(403)
            .body(Either::Right(Full::new(Bytes::from(
                "Denied by network rules\n",
            ))))
            .unwrap());
    }
    debug!("http allowed: {method} {}", http_req.path);

    let (parts, body) = req.into_parts();
    // h2 needs absolute URI for :authority/:scheme pseudo-headers.
    // h1 uses relative path + Host header.
    let uri_str = if h2 {
        let scheme = if connect.tls { "https" } else { "http" };
        format!(
            "{scheme}://{}:{}{}",
            http_req.connect.host, http_req.connect.port, http_req.path
        )
    } else {
        http_req.path.clone()
    };
    let uri: hyper::Uri = match uri_str.parse() {
        Ok(u) => u,
        Err(e) => {
            debug!("bad outgoing URI: {e}");
            return Ok(Response::builder()
                .status(400)
                .body(Either::Right(Full::new(Bytes::from("Bad request URI\n"))))
                .unwrap());
        }
    };
    let mut builder = Request::builder().method(parts.method.clone()).uri(&uri);
    for (name, value) in &http_req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let out_req = match builder.body(body) {
        Ok(r) => r,
        Err(e) => {
            debug!("failed to build outgoing request: {e}");
            return Ok(Response::builder()
                .status(502)
                .body(Either::Right(Full::new(Bytes::from("Bad gateway\n"))))
                .unwrap());
        }
    };

    debug!(
        "forwarding: {} {} (out uri={}) ({} headers)",
        parts.method,
        http_req.path,
        out_req.uri(),
        http_req.headers.len()
    );
    for (k, v) in &http_req.headers {
        trace!("  >> {k}: {}", safe_header_value(k, v));
    }

    let response_future = sender.send(out_req);
    let resp = match response_future.await {
        Ok(r) => r,
        Err(e) => {
            debug!("upstream error: {e}");
            return Err(e);
        }
    };

    debug!(
        "response: {} ({} headers)",
        resp.status(),
        resp.headers().len()
    );
    for (k, v) in resp.headers() {
        trace!(
            "  < {}: {}",
            k,
            safe_header_value(k.as_str(), v.to_str().unwrap_or("?"))
        );
    }
    let (parts, body) = resp.into_parts();
    Ok(Response::from_parts(parts, Either::Left(body)))
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
