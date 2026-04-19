//! TLS interception (MITM) for HTTP middleware.
//!
//! When a target has middleware attached, the proxy terminates the container's
//! TLS, inspects/modifies HTTP traffic, then re-encrypts to the real server.
//! Per-hostname leaf certificates are generated on demand and cached.

use std::sync::Arc;

use airlock_common::supervisor_capnp::tcp_sink;
use bytes::Bytes;
use quick_cache::sync::Cache;
use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, trace};

use super::io;
use crate::network::target::ResolvedTarget;

/// Accept a TLS handshake from the container (MITM) and wrap the decrypted
/// stream in a `Transport`. The container sees a valid cert for its intended
/// hostname signed by our CA. Returns the negotiated ALPN so the caller can
/// either match it when dialing the real server (allow path) or use it to
/// serve a denial (deny path) without reaching the upstream.
pub async fn accept_container(
    host: &str,
    first: Bytes,
    rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    interceptor: &TlsInterceptor,
) -> anyhow::Result<(io::Transport, Option<Bytes>)> {
    let sni_host = extract_sni(&first).unwrap_or_else(|| host.to_string());
    let rpc_io = io::RpcTransport::new(first, rx, client_sink);
    let (tls_stream, alpn) = tokio::time::timeout(
        crate::constants::TLS_HANDSHAKE_TIMEOUT,
        interceptor.accept(rpc_io, &sni_host),
    )
    .await
    .map_err(|_| anyhow::anyhow!("TLS handshake timeout"))??;

    let is_h2 = alpn.as_deref() == Some(b"h2");
    debug!(
        "tls accepted: {host} alpn={:?}",
        alpn.as_deref().map(String::from_utf8_lossy)
    );

    let (cr, cw) = tokio::io::split(tls_stream);
    Ok((
        io::Transport {
            read: Box::new(cr),
            write: Box::new(cw),
            h2: is_h2,
        },
        alpn,
    ))
}

/// Dial the real server over TLS with matching ALPN. Only called on the
/// allow path.
pub async fn connect_server(
    target: &ResolvedTarget,
    alpn: Option<&[u8]>,
    tls_client: &Arc<rustls::ClientConfig>,
) -> anyhow::Result<io::Transport> {
    let addr = format!("{}:{}", target.host, target.port);
    let server_stream = TcpStream::connect(&addr).await?;
    let mut config = (**tls_client).clone();
    config.alpn_protocols = match alpn {
        Some(proto) => vec![proto.to_vec()],
        None => vec![b"http/1.1".to_vec()],
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from(target.host.clone())
        .map_err(|e| anyhow::anyhow!("invalid hostname: {e}"))?;
    let server_tls = connector.connect(server_name, server_stream).await?;
    let server_h2 = server_tls.get_ref().1.alpn_protocol() == Some(b"h2");
    trace!("tls to server: {addr} h2={server_h2}");
    let (sr, sw) = tokio::io::split(server_tls);
    Ok(io::Transport {
        read: Box::new(sr),
        write: Box::new(sw),
        h2: server_h2,
    })
}

/// TLS interceptor with per-hostname cert caching.
pub struct TlsInterceptor {
    issuer: Issuer<'static, KeyPair>,
    cache: Cache<String, Arc<ServerConfig>>,
}

impl TlsInterceptor {
    /// Create an interceptor from the project CA certificate and private key.
    pub fn new(ca_cert_pem: &str, ca_key_pem: &str) -> anyhow::Result<Self> {
        let ca_key = KeyPair::from_pem(ca_key_pem)?;
        let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key)?;

        Ok(Self {
            issuer,
            cache: Cache::new(256),
        })
    }

    /// Perform TLS server-side handshake, returning the negotiated ALPN protocol.
    pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: S,
        hostname: &str,
    ) -> anyhow::Result<(tokio_rustls::server::TlsStream<S>, Option<Bytes>)> {
        let config = self.get_or_create_config(hostname)?;
        let acceptor = TlsAcceptor::from(config);
        let tls_stream = acceptor.accept(stream).await?;
        let alpn = tls_stream
            .get_ref()
            .1
            .alpn_protocol()
            .map(Bytes::copy_from_slice);
        Ok((tls_stream, alpn))
    }

    /// Get or generate a TLS server config with a leaf cert for `hostname`,
    /// signed by the project CA.
    fn get_or_create_config(&self, hostname: &str) -> anyhow::Result<Arc<ServerConfig>> {
        if let Some(config) = self.cache.get(hostname) {
            return Ok(config);
        }

        let leaf_key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![hostname.to_string()])?;
        params.not_before = rcgen::date_time_ymd(1970, 1, 1);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("airlock {hostname}"));
        let leaf_cert = params.signed_by(&leaf_key, &self.issuer)?;

        let cert_der = rustls::pki_types::CertificateDer::from(leaf_cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der())
            .map_err(|e| anyhow::anyhow!("key conversion: {e}"))?;

        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;
        // Offer both h2 and h1.1 — we'll match the container's choice when
        // connecting to the real server.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let config = Arc::new(config);

        self.cache.insert(hostname.to_string(), config.clone());
        Ok(config)
    }
}

/// Read from the channel until we can determine if the stream is TLS.
///
/// Returns `(true, buf)` if TLS handshake detected, `(false, buf)` otherwise.
/// The buffer always contains the consumed bytes for re-feeding as prefix.
pub async fn detect(rx: &mut mpsc::Receiver<Bytes>) -> (bool, Bytes) {
    use tls_parser::{TlsRecordType, parse_tls_record_header};

    let mut buf = bytes::BytesMut::new();

    // Read until we have at least 5 bytes (TLS record header)
    while buf.len() < 5 {
        let Some(data) = rx.recv().await else {
            return (false, buf.freeze());
        };
        buf.extend_from_slice(&data);
    }

    // Parse record header — check if it's a handshake record
    let hdr = match parse_tls_record_header(&buf) {
        Ok((_, hdr)) if hdr.record_type == TlsRecordType::Handshake => hdr,
        _ => return (false, buf.freeze()),
    };

    // Read until we have the full record (header + payload)
    let record_len = 5 + hdr.len as usize;
    while buf.len() < record_len {
        let Some(data) = rx.recv().await else {
            return (false, buf.freeze());
        };
        buf.extend_from_slice(&data);
    }

    (true, buf.freeze())
}

/// Extract SNI hostname from a buffered TLS ClientHello.
pub fn extract_sni(buf: &[u8]) -> Option<String> {
    use tls_parser::{
        TlsExtension, TlsMessage, TlsMessageHandshake, parse_tls_extensions, parse_tls_plaintext,
    };
    let (_, plaintext) = parse_tls_plaintext(buf).ok()?;
    for msg in &plaintext.msg {
        if let TlsMessage::Handshake(TlsMessageHandshake::ClientHello(ch)) = msg {
            let (_, extensions) = parse_tls_extensions(ch.ext?).ok()?;
            for ext in extensions {
                if let TlsExtension::SNI(sni_list) = ext {
                    for (sni_type, name) in sni_list {
                        if sni_type.0 == 0 {
                            return std::str::from_utf8(name).ok().map(String::from);
                        }
                    }
                }
            }
        }
    }
    None
}
