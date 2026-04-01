use quick_cache::sync::Cache;
use rcgen::{CertificateParams, KeyPair};
use rustls::ServerConfig;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsAcceptor;

/// TLS interceptor with per-hostname cert caching.
pub struct TlsInterceptor {
    ca_cert: rcgen::Certificate,
    ca_key: KeyPair,
    cache: Cache<String, Arc<ServerConfig>>,
}

impl TlsInterceptor {
    pub fn new(ca_cert_pem: &str, ca_key_pem: &str) -> anyhow::Result<Self> {
        let ca_key = KeyPair::from_pem(ca_key_pem)?;
        let ca_params = CertificateParams::from_ca_cert_pem(ca_cert_pem)?;
        let ca_cert = ca_params.self_signed(&ca_key)?;

        Ok(Self {
            ca_cert,
            ca_key,
            cache: Cache::new(256),
        })
    }

    pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: S,
        hostname: &str,
    ) -> anyhow::Result<tokio_rustls::server::TlsStream<S>> {
        let config = self.get_or_create_config(hostname)?;
        let acceptor = TlsAcceptor::from(config);
        Ok(acceptor.accept(stream).await?)
    }

    fn get_or_create_config(&self, hostname: &str) -> anyhow::Result<Arc<ServerConfig>> {
        if let Some(config) = self.cache.get(hostname) {
            return Ok(config);
        }

        let leaf_key = KeyPair::generate()?;
        let params = CertificateParams::new(vec![hostname.to_string()])?;
        let leaf_cert = params.signed_by(&leaf_key, &self.ca_cert, &self.ca_key)?;

        let cert_der = rustls::pki_types::CertificateDer::from(leaf_cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der())
            .map_err(|e| anyhow::anyhow!("key conversion: {e}"))?;

        let config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)?,
        );

        self.cache.insert(hostname.to_string(), config.clone());
        Ok(config)
    }
}

pub fn is_tls(buf: &[u8]) -> bool {
    !buf.is_empty() && buf[0] == 0x16
}

pub fn extract_sni(buf: &[u8]) -> Option<String> {
    if buf.len() < 5 || buf[0] != 0x16 {
        return None;
    }

    let hs = 5;
    if buf.len() < hs + 38 || buf[hs] != 0x01 {
        return None;
    }

    let mut pos = hs + 38;

    if pos >= buf.len() { return None; }
    pos += 1 + buf[pos] as usize;

    if pos + 2 > buf.len() { return None; }
    pos += 2 + u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;

    if pos >= buf.len() { return None; }
    pos += 1 + buf[pos] as usize;

    if pos + 2 > buf.len() { return None; }
    let ext_end = pos + 2 + u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
    pos += 2;

    while pos + 4 <= ext_end.min(buf.len()) {
        let ext_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let ext_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;

        if ext_type == 0 && pos + 5 <= buf.len() {
            let name_len = u16::from_be_bytes([buf[pos + 3], buf[pos + 4]]) as usize;
            if pos + 5 + name_len <= buf.len() {
                return String::from_utf8(buf[pos + 5..pos + 5 + name_len].to_vec()).ok();
            }
        }
        pos += ext_len;
    }
    None
}
