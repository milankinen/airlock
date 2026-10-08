//! Fake upstreams and the guest side of a request: an HTTP upstream
//! over TLS that records what it received, an upgrade-echo server, and a
//! guest HTTPS client that goes through the proxy.

use std::net::{SocketAddr, TcpListener as StdListener};
use std::sync::{Arc, Mutex};

use airlock_test_utils::{LocalExec, read_until_contains, serve_tls_on, server_tls, tls_trusting};
use axum::Router;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Request, StatusCode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpListener;

use super::network::TestConnection;
use crate::network::target::Endpoint;

/// Speak the upgrade-echo protocol on one accepted stream: answer a
/// well-formed WebSocket handshake with 101, or a `CONNECT` with 200, plus
/// an immediate greeting, then echo every byte read back upper-cased.
/// A `CONNECT` with `X-Reply: 204` gets a bare 204 and the connection is
/// then held open, idle. Anything else gets a 400.
///
/// Raw bytes rather than a WebSocket library so the tests see exactly
/// which bytes cross the proxy on both sides of the switch.
pub async fn upgrade_echo<S: AsyncRead + AsyncWrite + Unpin>(mut sock: S) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let body_start = loop {
        let n = sock.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
    let reply: &[u8] = if head.starts_with("connect ") && head.contains("x-reply: 204") {
        sock.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
            .await
            .unwrap();
        while sock.read(&mut chunk).await.is_ok_and(|n| n > 0) {}
        return;
    } else if head.starts_with("connect ") {
        b"HTTP/1.1 200 Connection Established\r\n\r\nserver-hello"
    } else if head.contains("upgrade: websocket")
        && head.contains("connection: upgrade")
        && head.contains("sec-websocket-key:")
    {
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
          Connection: Upgrade\r\nSec-WebSocket-Accept: test\r\n\r\nserver-hello"
    } else {
        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\n\r\nnot-upgrade"
    };
    sock.write_all(reply).await.unwrap();
    if reply.starts_with(b"HTTP/1.1 400") {
        return;
    }
    // Bytes the client sent right behind its request count too.
    let mut pending = buf[body_start..].to_vec();
    loop {
        if !pending.is_empty() {
            sock.write_all(&pending.to_ascii_uppercase()).await.unwrap();
            pending.clear();
        }
        let n = sock.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        pending.extend_from_slice(&chunk[..n]);
    }
}

/// Plain-TCP [`upgrade_echo`] server.
pub async fn serve_upgrade_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            tokio::spawn(upgrade_echo(sock));
        }
    });
    addr
}

/// A WebSocket handshake for [`upgrade_echo`]. `key` false leaves out
/// `Sec-WebSocket-Key`, which the server rejects.
pub fn websocket_handshake(port: u16, key: bool) -> String {
    let key = if key {
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    } else {
        ""
    };
    format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\n{key}Sec-WebSocket-Version: 13\r\n\r\n"
    )
}

/// Send `request` to an [`upgrade_echo`] server through the proxy on
/// `stream`, expect a reply starting with `status`, then relay raw bytes
/// both ways. The first client bytes ride in the same write as the
/// request and the server's greeting rides behind its reply, so both
/// hyper read buffers are exercised.
pub async fn assert_raw_relay<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    request: &str,
    status: &str,
) {
    stream
        .write_all(format!("{request}ping-1").as_bytes())
        .await
        .unwrap();
    let resp = read_until_contains(stream, "PING-1").await;
    let (head, rest) = resp.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with(status), "expected {status}, got: {head}");
    let head = head.to_lowercase();
    if status.contains("101") {
        assert!(
            head.contains("upgrade: websocket"),
            "missing Upgrade: {head}"
        );
        assert!(
            head.contains("connection: upgrade"),
            "Connection: upgrade must survive the proxy: {head}"
        );
    }
    assert!(
        !head.contains("connection: close"),
        "switch must not be rewritten to close: {head}"
    );
    assert_eq!(rest, "server-helloPING-1", "bytes behind the switch");

    stream.write_all(b"ping-2").await.unwrap();
    assert_eq!(read_until_contains(stream, "PING-2").await, "PING-2");
    stream.write_all(b"ping-3").await.unwrap();
    assert_eq!(read_until_contains(stream, "PING-3").await, "PING-3");
}

/// One request as the fake upstream received it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    /// The authority: `:authority` (h2) or `Host` (h1).
    pub authority: Option<String>,
    pub headers: HeaderMap,
    pub body: String,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Every request the fake upstream received, in order.
#[derive(Clone, Default)]
pub struct SeenLog(Arc<Mutex<Vec<Seen>>>);

impl SeenLog {
    pub fn all(&self) -> Vec<Seen> {
        self.0.lock().unwrap().clone()
    }

    pub fn last(&self) -> Seen {
        self.all().pop().expect("the upstream saw a request")
    }

    /// Record `req` and hand back what was seen.
    pub async fn record(&self, req: axum::extract::Request) -> Seen {
        let (parts, body) = req.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        let authority = parts.uri.authority().map(ToString::to_string).or_else(|| {
            parts
                .headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(String::from)
        });
        let seen = Seen {
            method: parts.method.to_string(),
            path: parts.uri.path().to_string(),
            authority,
            headers: parts.headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        };
        self.0.lock().unwrap().push(seen.clone());
        seen
    }
}

/// A fake upstream: a TLS listener on 127.0.0.1 with its own CA.
pub struct FakeUpstream {
    listener: StdListener,
    tls: Arc<rustls::ServerConfig>,
    ca_pem: String,
}

impl FakeUpstream {
    /// Bind now (outside the runtime), offering `alpn`.
    pub fn bind(alpn: &[&[u8]]) -> Self {
        let (tls, ca_pem) = server_tls(alpn);
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            listener,
            tls,
            ca_pem,
        }
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new("127.0.0.1", self.port())
    }

    /// A TLS client config that trusts this upstream: what the services'
    /// own token-endpoint client uses in the tests.
    pub fn client_tls(&self) -> Arc<rustls::ClientConfig> {
        Arc::new(tls_trusting(&self.ca_pem))
    }

    pub fn ca_pem(&self) -> String {
        self.ca_pem.clone()
    }

    /// Serve `app` (h1 or h2, by ALPN). Call inside the runtime.
    pub fn serve(self, app: Router) {
        serve_tls_on(TcpListener::from_std(self.listener).unwrap(), self.tls, app);
    }

    /// Serve the [`upgrade_echo`] protocol (a WebSocket endpoint), and
    /// keep every byte the upstream read in `read`.
    pub fn serve_upgrade_echo(self, read: Arc<Mutex<Vec<u8>>>) {
        let acceptor = tokio_rustls::TlsAcceptor::from(self.tls);
        let listener = TcpListener::from_std(self.listener).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (acceptor, read) = (acceptor.clone(), read.clone());
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        upgrade_echo(Recording { inner: tls, read }).await;
                    }
                });
            }
        });
    }
}

/// A stream that keeps a copy of everything read from it.
struct Recording<S> {
    inner: S,
    read: Arc<Mutex<Vec<u8>>>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Recording<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        let new = buf.filled()[before..].to_vec();
        self.read.lock().unwrap().extend(new);
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Recording<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A response as the guest got it.
pub struct GuestResponse {
    pub status: StatusCode,
    pub body: String,
}

impl GuestResponse {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

/// The guest's side: TLS to `127.0.0.1:port` through the proxy (trusting
/// the sandbox CA), one request over h1 or h2. A relative URI gets the
/// connection's authority (h2), and a request without `Host` gets it as
/// `Host` (h1).
pub async fn guest_request(
    proxy: &airlock_common::network_capnp::network_proxy::Client,
    mitm_ca: &str,
    port: u16,
    h2: bool,
    req: Request<Full<Bytes>>,
) -> GuestResponse {
    let conn = TestConnection::connect(proxy, "127.0.0.1", port)
        .await
        .expect("the proxy accepts the connection");
    let mut config = tls_trusting(mitm_ca);
    config.alpn_protocols = vec![if h2 {
        b"h2".to_vec()
    } else {
        b"http/1.1".to_vec()
    }];
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
            conn.into_stream(),
        )
        .await
        .unwrap();
    let io = hyper_util::rt::TokioIo::new(tls);
    let resp = if h2 {
        let (mut sender, conn) = hyper::client::conn::http2::handshake(LocalExec, io)
            .await
            .unwrap();
        tokio::task::spawn_local(conn);
        let (mut parts, body) = req.into_parts();
        if parts.uri.authority().is_none() {
            parts.uri = format!("https://127.0.0.1:{port}{}", parts.uri)
                .parse()
                .unwrap();
        }
        sender
            .send_request(Request::from_parts(parts, body))
            .await
            .unwrap()
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
        tokio::task::spawn_local(conn);
        let mut req = req;
        req.headers_mut()
            .entry("host")
            .or_insert(format!("127.0.0.1:{port}").parse().unwrap());
        sender.send_request(req).await.unwrap()
    };
    let (parts, body) = resp.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    GuestResponse {
        status: parts.status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// A request with a JSON or form body.
pub fn post(path: &str, content_type: &str, body: &str) -> Request<Full<Bytes>> {
    Request::post(path)
        .header("content-type", content_type)
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

/// A GET with an `Authorization: Bearer` header.
pub fn get_with_bearer(path: &str, token: &str) -> Request<Full<Bytes>> {
    Request::get(path)
        .header("authorization", format!("Bearer {token}"))
        .body(Full::new(Bytes::new()))
        .unwrap()
}
