use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;

/// A self-signed CA for a test, in PEM, with its key.
pub struct TestCa {
    pub cert_pem: String,
    pub key_pem: String,
}

impl TestCa {
    pub fn generate() -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        Self {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        }
    }

    /// A server config with a leaf for `127.0.0.1` signed by this CA,
    /// offering `alpn`.
    pub fn server_tls(&self, alpn: &[&[u8]]) -> Arc<rustls::ServerConfig> {
        let ca_key = rcgen::KeyPair::from_pem(&self.key_pem).unwrap();
        let issuer = rcgen::Issuer::from_ca_cert_pem(&self.cert_pem, ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".into()]).unwrap();
        params.not_before = rcgen::date_time_ymd(1970, 1, 1);
        let leaf = params.signed_by(&leaf_key, &issuer).unwrap();

        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap(),
            )
            .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Arc::new(config)
    }
}

/// A TLS server config for `127.0.0.1` offering `alpn`, and the PEM of
/// the fresh CA that signed it.
pub fn server_tls(alpn: &[&[u8]]) -> (Arc<rustls::ServerConfig>, String) {
    let ca = TestCa::generate();
    (ca.server_tls(alpn), ca.cert_pem)
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

/// Serve `app` over TLS on `listener` (HTTP/1.1 or HTTP/2, by ALPN) until
/// the runtime ends.
pub fn serve_tls_on(
    listener: tokio::net::TcpListener,
    tls: Arc<rustls::ServerConfig>,
    app: Router,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, app) = (acceptor.clone(), app.clone());
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    crate::serve_connection(tls, app).await;
                }
            });
        }
    });
}

/// Serve `app` over TLS on an ephemeral `127.0.0.1` port.
pub async fn serve_tls(app: Router, tls: Arc<rustls::ServerConfig>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    serve_tls_on(listener, tls, app);
    addr
}
