use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::helpers::*;

/// Pre-generate a server CA + leaf cert with configurable ALPN.
fn make_server_tls_with_alpn(alpn: Vec<Vec<u8>>) -> (Arc<rustls::ServerConfig>, String) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_pem = ca_cert.pem();
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca_pem, ca_key).unwrap();

    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let mut leaf_params = rcgen::CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
    leaf_params.not_before = rcgen::date_time_ymd(1970, 1, 1);
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    let cert_der = rustls::pki_types::CertificateDer::from(leaf_cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    config.alpn_protocols = alpn;
    (Arc::new(config), ca_pem)
}

fn make_server_tls() -> (Arc<rustls::ServerConfig>, String) {
    make_server_tls_with_alpn(vec![b"http/1.1".to_vec()])
}

/// Start an HTTPS server with a pre-built TLS config. Must be called inside the tokio runtime.
async fn serve_https(app: Router, tls_config: Arc<rustls::ServerConfig>) -> std::net::SocketAddr {
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(tls_stream) = acceptor.accept(stream).await else {
                    return;
                };
                let io = hyper_util::rt::TokioIo::new(tls_stream);
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let mut app = app.clone();
                        async move {
                            use tower::Service;
                            app.call(req).await.map_err(|e| match e {})
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    addr
}

/// Do a full HTTP request over TLS through the RPC stream.
/// Returns (response_body, negotiated_alpn).
async fn tls_roundtrip_with_alpn(
    stream: RpcStream,
    ca_pem: &str,
    host: &str,
    port: u16,
    path: &str,
    client_alpn: Vec<Vec<u8>>,
) -> (String, Option<Vec<u8>>) {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
        root_store.add(cert.unwrap()).unwrap();
    }
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = client_alpn;

    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string()).unwrap();
    let tls_stream = connector.connect(server_name, stream).await.unwrap();
    let alpn = tls_stream.get_ref().1.alpn_protocol().map(Vec::from);

    let mut tls_stream = tls_stream;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    tls_stream.write_all(req.as_bytes()).await.unwrap();

    let mut buf = Vec::new();
    let _ = tls_stream.read_to_end(&mut buf).await;
    (String::from_utf8_lossy(&buf).into_owned(), alpn)
}

async fn tls_roundtrip(
    stream: RpcStream,
    ca_pem: &str,
    host: &str,
    port: u16,
    path: &str,
) -> String {
    tls_roundtrip_with_alpn(stream, ca_pem, host, port, path, vec![b"http/1.1".to_vec()])
        .await
        .0
}

#[test]
fn tls_mitm_basic() {
    // Pre-generate server certs (before runtime starts)
    let (server_tls, server_ca_pem) = make_server_tls();

    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![server_ca_pem],
            ..Default::default()
        },
        |proxy, _log, mitm_ca_pem| async move {
            let addr = serve_https(
                Router::new().route("/", get(|| async { "tls-ok" })),
                server_tls,
            )
            .await;

            let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .expect("should connect");
            let resp = tls_roundtrip(
                conn.into_stream(),
                &mitm_ca_pem,
                "127.0.0.1",
                addr.port(),
                "/",
            )
            .await;
            assert!(resp.contains("200"), "expected 200: {resp}");
            assert!(resp.contains("tls-ok"), "expected body: {resp}");
        },
    );
}

#[test]
fn tls_mitm_with_middleware() {
    let (server_tls, server_ca_pem) = make_server_tls();

    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![server_ca_pem],
            middleware_scripts: vec![(
                "tls inject",
                r#"req:setHeader("x-tls-injected", "from-lua-over-tls")"#,
            )],
            ..Default::default()
        },
        |proxy, _log, mitm_ca_pem| async move {
            let addr = serve_https(
                Router::new().route(
                    "/",
                    get(|headers: axum::http::HeaderMap| async move {
                        headers
                            .get("x-tls-injected")
                            .map_or("missing".into(), |v| v.to_str().unwrap().to_string())
                    }),
                ),
                server_tls,
            )
            .await;

            let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = tls_roundtrip(
                conn.into_stream(),
                &mitm_ca_pem,
                "127.0.0.1",
                addr.port(),
                "/",
            )
            .await;
            assert!(resp.contains("200"), "expected 200: {resp}");
            assert!(
                resp.contains("from-lua-over-tls"),
                "expected injected header: {resp}"
            );
        },
    );
}

// ── ALPN tests ──────────────────────────────────────────

#[test]
fn alpn_container_h1_server_h1() {
    let (server_tls, server_ca_pem) = make_server_tls_with_alpn(vec![b"http/1.1".to_vec()]);

    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![server_ca_pem],
            ..Default::default()
        },
        |proxy, _log, mitm_ca_pem| async move {
            let addr = serve_https(
                Router::new().route("/", get(|| async { "h1-ok" })),
                server_tls,
            )
            .await;
            let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let (resp, alpn) = tls_roundtrip_with_alpn(
                conn.into_stream(),
                &mitm_ca_pem,
                "127.0.0.1",
                addr.port(),
                "/",
                vec![b"http/1.1".to_vec()],
            )
            .await;
            assert!(resp.contains("h1-ok"), "expected body: {resp}");
            assert_eq!(
                alpn.as_deref(),
                Some(b"http/1.1".as_ref()),
                "ALPN should be h1"
            );
        },
    );
}

#[test]
fn alpn_container_h1_server_offers_h2_and_h1() {
    // Server offers h2 + h1, but container only offers h1.
    // CLI should match container's h1, NOT upgrade to h2.
    let (server_tls, server_ca_pem) =
        make_server_tls_with_alpn(vec![b"h2".to_vec(), b"http/1.1".to_vec()]);

    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![server_ca_pem],
            ..Default::default()
        },
        |proxy, _log, mitm_ca_pem| async move {
            let addr = serve_https(
                Router::new().route("/", get(|| async { "matched-h1" })),
                server_tls,
            )
            .await;
            let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            // Container offers only h1
            let (resp, alpn) = tls_roundtrip_with_alpn(
                conn.into_stream(),
                &mitm_ca_pem,
                "127.0.0.1",
                addr.port(),
                "/",
                vec![b"http/1.1".to_vec()],
            )
            .await;
            assert!(resp.contains("matched-h1"), "expected body: {resp}");
            // MITM should have negotiated h1 with the container
            assert_eq!(
                alpn.as_deref(),
                Some(b"http/1.1".as_ref()),
                "should match container's h1"
            );
        },
    );
}

#[test]
fn alpn_container_no_alpn() {
    // Container doesn't offer any ALPN — should default to h1.
    let (server_tls, server_ca_pem) =
        make_server_tls_with_alpn(vec![b"h2".to_vec(), b"http/1.1".to_vec()]);

    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![server_ca_pem],
            ..Default::default()
        },
        |proxy, _log, mitm_ca_pem| async move {
            let addr = serve_https(
                Router::new().route("/", get(|| async { "no-alpn-ok" })),
                server_tls,
            )
            .await;
            let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            // Container offers NO ALPN
            let (resp, alpn) = tls_roundtrip_with_alpn(
                conn.into_stream(),
                &mitm_ca_pem,
                "127.0.0.1",
                addr.port(),
                "/",
                vec![], // no ALPN
            )
            .await;
            assert!(resp.contains("no-alpn-ok"), "expected body: {resp}");
            // No ALPN negotiated on the MITM side
            assert_eq!(alpn, None, "no ALPN should be negotiated");
        },
    );
}

#[test]
fn alpn_container_h2_server_h2() {
    // Both sides support h2 — the MITM should negotiate h2 on both sides.
    // Note: we can't actually send h2 frames manually (it's a binary protocol),
    // so we just verify the ALPN negotiation succeeds. The TLS handshake
    // would fail if there was a mismatch.
    let (server_tls, server_ca_pem) = make_server_tls_with_alpn(vec![b"h2".to_vec()]);

    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![server_ca_pem],
            ..Default::default()
        },
        |proxy, _log, mitm_ca_pem| async move {
            let addr = serve_https(
                Router::new().route("/", get(|| async { "h2-ok" })),
                server_tls,
            )
            .await;
            let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();

            // Container offers h2
            let mut root_store = rustls::RootCertStore::empty();
            for cert in rustls_pemfile::certs(&mut mitm_ca_pem.as_bytes()) {
                root_store.add(cert.unwrap()).unwrap();
            }
            let mut tls_config = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            tls_config.alpn_protocols = vec![b"h2".to_vec()];

            let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
            let server_name =
                rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
            let tls_stream = connector
                .connect(server_name, conn.into_stream())
                .await
                .unwrap();
            let alpn = tls_stream.get_ref().1.alpn_protocol().map(Vec::from);

            // Verify h2 was negotiated on the MITM side
            assert_eq!(alpn.as_deref(), Some(b"h2".as_ref()), "ALPN should be h2");
        },
    );
}
