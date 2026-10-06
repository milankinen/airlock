//! Plumbing for the network-service tests: a fake provider upstream over
//! TLS (h1 and h2, bound before the runtime starts so the service can be
//! built with its port), a token store in a temp home, and a guest-side
//! HTTPS client that goes through the proxy.

use std::net::TcpListener as StdListener;
use std::sync::{Arc, Mutex};

use axum::Router;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Request, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::helpers::*;
use super::test_tls::{LocalExec, make_server_tls_with_alpn};
use crate::db::Db;
use crate::network::target::Endpoint;
use crate::services::store::TokenStore;
use crate::test_support::test_context;
use crate::vault::{Vault, VaultStorageType};

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
        let (tls, ca_pem) = make_server_tls_with_alpn(alpn.iter().map(|p| p.to_vec()).collect());
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
        let acceptor = tokio_rustls::TlsAcceptor::from(self.tls);
        let listener = tokio::net::TcpListener::from_std(self.listener).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (acceptor, app) = (acceptor.clone(), app.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    let svc = hyper::service::service_fn(move |req| {
                        let mut app = app.clone();
                        async move {
                            use tower::Service;
                            app.call(req).await.map_err(|e| match e {})
                        }
                    });
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                    .await;
                });
            }
        });
    }

    /// Serve the [`upgrade_echo`] protocol (a WebSocket endpoint), and
    /// keep every byte the upstream read in `read`.
    pub fn serve_upgrade_echo(self, read: Arc<Mutex<Vec<u8>>>) {
        let acceptor = tokio_rustls::TlsAcceptor::from(self.tls);
        let listener = tokio::net::TcpListener::from_std(self.listener).unwrap();
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

/// A TLS client config trusting only `ca_pem`.
pub fn tls_trusting(ca_pem: &str) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// The temp home of a [`test_store`] and its database.
pub struct StoreHome {
    _dir: tempfile::TempDir,
    pub db: Db,
}

/// A token store in the database of a test context in a fresh temp home
/// (kept alive by the returned [`StoreHome`]).
pub fn test_store() -> (StoreHome, Arc<TokenStore>) {
    let dir = tempfile::tempdir().unwrap();
    let context = test_context(
        dir.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let store = TokenStore::new(context.db.clone(), &[42; 32]);
    let home = StoreHome {
        _dir: dir,
        db: context.db,
    };
    (home, Arc::new(store))
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
