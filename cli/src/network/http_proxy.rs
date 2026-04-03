//! HTTP request interception via hyper.
//!
//! When the first bytes from the container look like HTTP, we hand off
//! to hyper's auto-detecting HTTP server (h1/h2) and h1/h2 client.
//! For each request, Lua scripts run and the (possibly modified) request
//! is forwarded via hyper client. Bodies are streamed, not buffered.

use super::scripting::{HttpRequestInfo, ScriptEngine, TcpConnect};
use ezpez_protocol::supervisor_capnp::tcp_sink;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use http_body_util::{Either, Full};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tracing::{debug, trace};

const MAX_DETECT_SIZE: usize = 4096;

/// Peek at the first request line to detect HTTP.
///
/// Buffers up to 4KB or until the first `\r\n`, then checks if the line
/// matches `METHOD path HTTP/x.y\r\n`. Returns `Ok(buf)` if HTTP,
/// `Err(buf)` if not.
pub async fn detect(rx: &mut mpsc::Receiver<Vec<u8>>) -> Result<Vec<u8>, Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        let Some(data) = rx.recv().await else {
            trace!("channel closed before HTTP detection ({} bytes)", buf.len());
            return Err(buf);
        };
        buf.extend_from_slice(&data);

        // Look for the first line ending
        if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
            if is_http_request_line(&buf[..pos]) {
                debug!("detected HTTP request line");
                return Ok(buf);
            } else {
                trace!("first line is not HTTP: {:?}", String::from_utf8_lossy(&buf[..pos.min(80)]));
                return Err(buf);
            }
        }

        if buf.len() > MAX_DETECT_SIZE {
            trace!("no linebreak in first {}B, not HTTP", MAX_DETECT_SIZE);
            return Err(buf);
        }
    }
}


/// Whether the upstream server speaks h1 or h2.
#[derive(Debug, Clone, Copy)]
pub enum ServerProtocol {
    Http1,
    Http2,
}

/// Run hyper HTTP proxy with auto h1/h2 detection. Bodies stream through.
pub async fn serve<R, W>(
    prefix: Vec<u8>,
    rx: mpsc::Receiver<Vec<u8>>,
    client_sink: tcp_sink::Client,
    server_read: R,
    server_write: W,
    server_protocol: ServerProtocol,
    engine: &Rc<ScriptEngine>,
    connect: &TcpConnect,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let container_io = RpcTransport::new(prefix, rx, client_sink.clone());
    let container_conn = hyper_util::rt::TokioIo::new(container_io);

    let server_io = hyper_util::rt::TokioIo::new(CombinedStream { read: server_read, write: server_write });
    debug!("http proxy: server protocol = {server_protocol:?}");
    let sender: Rc<dyn RequestSender> = match server_protocol {
        ServerProtocol::Http1 => {
            let (sender, conn) = hyper::client::conn::http1::handshake(server_io).await?;
            tokio::task::spawn_local(conn);
            debug!("h1 client handshake complete");
            Rc::new(H1Sender(RefCell::new(sender)))
        }
        ServerProtocol::Http2 => {
            let (sender, conn) = hyper::client::conn::http2::handshake(LocalExec, server_io).await?;
            tokio::task::spawn_local(conn);
            debug!("h2 client handshake complete");
            Rc::new(H2Sender(sender))
        }
    };

    let engine = engine.clone();
    let connect = connect.clone();

    let service = service_fn(move |req: Request<Incoming>| {
        let engine = engine.clone();
        let connect = connect.clone();
        let sender = sender.clone();
        async move { handle_request(req, &engine, &connect, sender).await }
    });

    hyper_util::server::conn::auto::Builder::new(LocalExec)
        .serve_connection(container_conn, service)
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
    fn send(&self, req: Request<Incoming>) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>>;
}

struct H1Sender(RefCell<hyper::client::conn::http1::SendRequest<Incoming>>);
impl RequestSender for H1Sender {
    fn send(&self, req: Request<Incoming>) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        Box::pin(self.0.borrow_mut().send_request(req))
    }
}

/// h2 SendRequest is Clone and safe for concurrent use — clone per request.
struct H2Sender(hyper::client::conn::http2::SendRequest<Incoming>);
impl RequestSender for H2Sender {
    fn send(&self, req: Request<Incoming>) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        let mut sender = self.0.clone();
        Box::pin(async move { sender.send_request(req).await })
    }
}

type ResponseBody = Either<Incoming, Full<Bytes>>;

fn safe_header_value<'a>(name: &str, value: &'a str) -> &'a str {
    const SENSITIVE: &[&str] = &["authorization", "cookie", "set-cookie", "proxy-authorization"];
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
) -> Result<Response<ResponseBody>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers: Vec<(String, String)> = req.headers().iter()
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
        allowed: engine.default_allows(),
        denied: false,
    };

    if let Err(e) = engine.intercept_http_request(&mut http_req) {
        debug!("http denied: {method} {} ({e})", http_req.path);
        return Ok(Response::builder()
            .status(403)
            .body(Either::Right(Full::new(Bytes::from("Denied by network rules\n"))))
            .unwrap());
    }
    debug!("http allowed: {method} {}", http_req.path);

    let (parts, body) = req.into_parts();
    let scheme = if connect.tls { "https" } else { "http" };
    let authority = &http_req.connect.host;
    let path = &http_req.path;
    let uri: hyper::Uri = match format!("{scheme}://{authority}{path}").parse() {
        Ok(u) => u,
        Err(e) => {
            debug!("bad outgoing URI: {e}");
            return Ok(Response::builder()
                .status(400)
                .body(Either::Right(Full::new(Bytes::from("Bad request URI\n"))))
                .unwrap());
        }
    };
    let mut builder = Request::builder()
        .method(parts.method.clone())
        .uri(&uri);
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

    debug!("forwarding: {} {} (out uri={}) ({} headers)",
        parts.method, http_req.path, out_req.uri(), http_req.headers.len());
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

    debug!("response: {} ({} headers)", resp.status(), resp.headers().len());
    for (k, v) in resp.headers() {
        trace!("  < {}: {}", k, safe_header_value(k.as_str(), v.to_str().unwrap_or("?")));
    }
    let (parts, body) = resp.into_parts();
    Ok(Response::from_parts(parts, Either::Left(body)))
}

// -- I/O bridges --

struct CombinedStream<R, W> { read: R, write: W }

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for CombinedStream<R, W> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for CombinedStream<R, W> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}

struct RpcTransport {
    prefix: Vec<u8>,
    prefix_pos: usize,
    rx: mpsc::Receiver<Vec<u8>>,
    client_sink: tcp_sink::Client,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl RpcTransport {
    fn new(prefix: Vec<u8>, rx: mpsc::Receiver<Vec<u8>>, client_sink: tcp_sink::Client) -> Self {
        Self { prefix, prefix_pos: 0, rx, client_sink, read_buf: Vec::new(), read_pos: 0 }
    }
}

impl AsyncRead for RpcTransport {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.prefix_pos < self.prefix.len() {
            let remaining = &self.prefix[self.prefix_pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.prefix_pos += n;
            return Poll::Ready(Ok(()));
        }
        if self.read_pos < self.read_buf.len() {
            let remaining = &self.read_buf[self.read_pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.read_pos += n;
            if self.read_pos >= self.read_buf.len() {
                self.read_buf.clear();
                self.read_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    self.read_buf = data[n..].to_vec();
                    self.read_pos = 0;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for RpcTransport {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        // TODO: fire-and-forget — no backpressure or error propagation from RPC send.
        // With h2 concurrent streams this could silently drop data.
        let mut req = self.client_sink.send_request();
        req.get().set_data(buf);
        let _ = req.send();
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Check if a line matches an HTTP request line or h2 connection preface.
fn is_http_request_line(line: &[u8]) -> bool {
    use std::sync::LazyLock;
    use regex::bytes::Regex;

    static H2_PREFACE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^PRI \* HTTP/2\.0$").unwrap()
    });
    static H1_REQUEST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[A-Z]+ \S+ HTTP/\S+$").unwrap()
    });

    H2_PREFACE.is_match(line) || H1_REQUEST.is_match(line)
}
