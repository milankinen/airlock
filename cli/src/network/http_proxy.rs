//! HTTP request interception via hyper.
//!
//! When the first bytes from the container look like HTTP, we hand off
//! to hyper's HTTP/1.1 server (container side) and client (server side).
//! For each request, Lua scripts run and the (possibly modified) request
//! is forwarded via hyper client. Bodies are streamed, not buffered.

use super::scripting::{HttpRequestInfo, ScriptEngine, TcpConnect};
use ezpez_protocol::supervisor_capnp::tcp_sink;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use http_body_util::{Either, Full};
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tracing::{debug, trace};

/// Peek at first bytes from the channel to detect HTTP.
pub async fn detect(rx: &mut mpsc::Receiver<Vec<u8>>) -> Result<Vec<u8>, Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        let Some(data) = rx.recv().await else {
            trace!("channel closed before HTTP detection ({} bytes)", buf.len());
            return Err(buf);
        };
        buf.extend_from_slice(&data);

        if buf.len() >= 4 {
            if looks_like_http(&buf) {
                debug!("detected HTTP stream");
                return Ok(buf);
            } else {
                trace!("first bytes don't look like HTTP");
                return Err(buf);
            }
        }
    }
}

/// Run hyper HTTP/1.1 proxy. Bodies stream through without buffering.
pub async fn serve<R, W>(
    prefix: Vec<u8>,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    client_sink: tcp_sink::Client,
    server_read: R,
    server_write: W,
    engine: &Rc<ScriptEngine>,
    connect: &TcpConnect,
) -> Result<(), hyper::Error>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let container_io = RpcTransport::new(prefix, rx, client_sink);
    let container_conn = hyper_util::rt::TokioIo::new(container_io);

    let server_io = hyper_util::rt::TokioIo::new(CombinedStream { read: server_read, write: server_write });
    let (sender, server_conn) = hyper::client::conn::http1::handshake(server_io).await?;
    tokio::task::spawn_local(server_conn);
    let sender = Rc::new(RefCell::new(sender));

    let engine = engine.clone();
    let connect = connect.clone();

    let service = service_fn(move |req: Request<Incoming>| {
        let engine = engine.clone();
        let connect = connect.clone();
        let sender = sender.clone();
        async move { handle_request(req, &engine, &connect, sender).await }
    });

    http1::Builder::new()
        .serve_connection(container_conn, service)
        .await
}

/// Response body: either streamed from upstream or synthetic (deny).
type ResponseBody = Either<Incoming, Full<Bytes>>;

async fn handle_request(
    req: Request<Incoming>,
    engine: &ScriptEngine,
    connect: &TcpConnect,
    sender: Rc<RefCell<hyper::client::conn::http1::SendRequest<Incoming>>>,
) -> Result<Response<ResponseBody>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers: Vec<(String, String)> = req.headers().iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    debug!("http: {method} {path}");

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

    // Build outgoing request: modified headers + streaming body passthrough
    let (parts, body) = req.into_parts();
    let uri: hyper::Uri = http_req.path.parse().unwrap_or_else(|_| "/".parse().unwrap());
    let mut builder = Request::builder()
        .method(parts.method)
        .uri(uri);
    for (name, value) in &http_req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let out_req = builder.body(body).unwrap();

    // Forward — body streams through without buffering
    let resp = sender.borrow_mut().send_request(out_req).await?;
    trace!("response: {}", resp.status());
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

struct RpcTransport<'a> {
    prefix: Vec<u8>,
    prefix_pos: usize,
    rx: &'a mut mpsc::Receiver<Vec<u8>>,
    client_sink: tcp_sink::Client,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl<'a> RpcTransport<'a> {
    fn new(prefix: Vec<u8>, rx: &'a mut mpsc::Receiver<Vec<u8>>, client_sink: tcp_sink::Client) -> Self {
        Self { prefix, prefix_pos: 0, rx, client_sink, read_buf: Vec::new(), read_pos: 0 }
    }
}

impl AsyncRead for RpcTransport<'_> {
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

impl AsyncWrite for RpcTransport<'_> {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
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

fn looks_like_http(buf: &[u8]) -> bool {
    const METHODS: &[&[u8]] = &[
        b"GET ", b"POST", b"PUT ", b"DELE", b"HEAD",
        b"OPTI", b"PATC", b"CONN", b"TRAC",
    ];
    METHODS.iter().any(|m| buf.starts_with(m))
}
