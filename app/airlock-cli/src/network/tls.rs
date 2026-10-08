//! TLS interception (MITM).
//!
//! Detects TLS traffic, ends the TLS session from the sandbox, and opens a new
//! TLS session to the real server. The proxy can then read and change the
//! plaintext HTTP traffic between them.

use std::rc::Rc;
use std::sync::Arc;

use airlock_common::network_capnp::tcp_sink;
use bytes::Bytes;
use quick_cache::sync::Cache;
use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, trace};

use super::{io, tcp, traffic};
use crate::network::target::ResolvedTarget;

/// Accept a TLS handshake from the container (MITM) and wrap the decrypted
/// stream in a [`io::Transport`]. The container sees a valid certificate for
/// its intended host name, signed by the sandbox CA.
/// Args:
///  - `host`: Target host. Used if the ClientHello has no SNI.
///  - `first`: Bytes already read from the container (the ClientHello)
///  - `rx`: Receiver for more container bytes
///  - `client_sink`: RPC sink for bytes to the container
///  - `interceptor`: TLS interceptor that makes the certificates
///  - `counter`: Optional byte counter for the Monitor tab.
///
/// Returns:
///   The decrypted transport and the negotiated ALPN. On the allow path,
///   the caller uses the same ALPN to connect to the real server. The deny
///   path does not use the ALPN.
pub async fn accept_container(
    host: &str,
    first: Bytes,
    rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    interceptor: &TlsInterceptor,
    counter: Option<&Rc<traffic::TrafficCounter>>,
) -> anyhow::Result<(io::Transport, Option<Bytes>)> {
    let sni_host = extract_sni(&first).unwrap_or_else(|| host.to_string());
    let rpc_io = io::RpcTransport::new(first, rx, client_sink);
    // Count bytes *below* the TLS layer, so the Monitor tab shows wire
    // bytes: encrypted records and the handshake. `first` goes through this
    // stream again, so the count includes the ClientHello.
    // A branch on the counter (not an `Option` in the wrapper) keeps the
    // path with no monitor free of extra layers. The cost is one more
    // monomorphization of `handshake`.
    match counter {
        Some(c) => {
            handshake(
                traffic::count_stream(rpc_io, c),
                &sni_host,
                interceptor,
                host,
            )
            .await
        }
        None => handshake(rpc_io, &sni_host, interceptor, host).await,
    }
}

/// Terminate the container TLS on `stream`, with a timeout.
/// Args:
///  - `stream`: Raw container stream
///  - `sni_host`: Host name for the leaf certificate
///  - `interceptor`: TLS interceptor that makes the certificates
///  - `host`: Target host, for logs.
///
/// Returns:
///   The decrypted halves in a boxed [`io::Transport`], and the negotiated
///   ALPN.
async fn handshake<S: AsyncRead + AsyncWrite + Unpin + 'static>(
    stream: S,
    sni_host: &str,
    interceptor: &TlsInterceptor,
    host: &str,
) -> anyhow::Result<(io::Transport, Option<Bytes>)> {
    let (tls_stream, alpn) = tokio::time::timeout(
        crate::constants::TLS_HANDSHAKE_TIMEOUT,
        interceptor.accept(stream, sni_host),
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

/// Connect to the real server with TLS and a matching ALPN. Call it only
/// on the allow path.
/// Args:
///  - `target`: Allowed target
///  - `alpn`: ALPN that the container negotiated, if any
///  - `tls_client`: TLS client config for upstream connections.
///
/// Returns:
///   Server transport. Its `h2` flag shows the ALPN that the server
///   selected.
pub async fn connect_server(
    target: &ResolvedTarget,
    alpn: Option<&[u8]>,
    tls_client: &Arc<rustls::ClientConfig>,
) -> anyhow::Result<io::Transport> {
    let addr = format!("{}:{}", target.host, target.port);
    let server_stream = tcp::dial(target).await?;
    let mut config = (**tls_client).clone();
    // Offer the protocol of the container first, but always keep http/1.1
    // as a fallback. The proxy offers h2 to the container before it knows
    // what the upstream supports. If the proxy offers only h2, an
    // http/1.1-only upstream stops the handshake with
    // `no_application_protocol`. The guest then gets a bare FIN in the
    // middle of TLS ("unexpected eof").
    // The two sides can use different protocols. The HTTP relay selects its
    // upstream client from the protocol of the *server*.
    config.alpn_protocols = match alpn {
        Some(proto) if proto != b"http/1.1" => vec![proto.to_vec(), b"http/1.1".to_vec()],
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

/// TLS interceptor that makes a leaf certificate for each host name when
/// necessary, and caches it.
pub struct TlsInterceptor {
    issuer: Issuer<'static, KeyPair>,
    cache: Cache<String, Arc<ServerConfig>>,
}

impl TlsInterceptor {
    /// Make an interceptor from the PEM-encoded project CA certificate and
    /// private key.
    /// Returns:
    ///   The interceptor, or error if the PEM data is not valid.
    pub fn new(ca_cert_pem: &str, ca_key_pem: &str) -> anyhow::Result<Self> {
        let ca_key = KeyPair::from_pem(ca_key_pem)?;
        let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key)?;

        Ok(Self {
            issuer,
            cache: Cache::new(256),
        })
    }

    /// Do the server-side TLS handshake with a leaf certificate for
    /// `hostname`.
    /// Returns:
    ///   The TLS stream and the negotiated ALPN protocol.
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

    /// Get or make a TLS server config with a leaf certificate for
    /// `hostname`, signed by the project CA.
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
        // Offer h2 and h1.1. The connection to the real server uses the
        // selection of the container.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let config = Arc::new(config);

        self.cache.insert(hostname.to_string(), config.clone());
        Ok(config)
    }
}

/// Read from the channel until it is known if the stream is TLS.
/// Returns:
///   `(true, buf)` if the stream starts with a TLS handshake record,
///   `(false, buf)` if not. `buf` always contains the bytes that were read,
///   so the caller can use them again as a prefix.
pub async fn detect(rx: &mut mpsc::Receiver<Bytes>) -> (bool, Bytes) {
    use tls_parser::{TlsRecordType, parse_tls_record_header};

    let mut buf = bytes::BytesMut::new();

    // Read at least 5 bytes (the TLS record header).
    while buf.len() < 5 {
        let Some(data) = rx.recv().await else {
            return (false, buf.freeze());
        };
        buf.extend_from_slice(&data);
    }

    // Parse the record header and make sure that it is a handshake record.
    let hdr = match parse_tls_record_header(&buf) {
        Ok((_, hdr)) if hdr.record_type == TlsRecordType::Handshake => hdr,
        _ => return (false, buf.freeze()),
    };

    // Read the full record (header and payload).
    let record_len = 5 + hdr.len as usize;
    while buf.len() < record_len {
        let Some(data) = rx.recv().await else {
            return (false, buf.freeze());
        };
        buf.extend_from_slice(&data);
    }

    (true, buf.freeze())
}

/// Get the SNI host name from a buffered TLS ClientHello.
/// Returns:
///   The host name, or `None` if the buffer has no ClientHello with SNI.
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
