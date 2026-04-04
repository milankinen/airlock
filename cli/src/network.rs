mod http_proxy;
pub mod scripting;
mod server;

use std::rc::Rc;
use std::sync::Arc;

use scripting::ScriptEngine;

use crate::project::Project;

pub fn setup(project: &Project) -> anyhow::Result<Network> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("native certs") {
        let _ = root_store.add(cert);
    }

    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let script_engine = Rc::new(ScriptEngine::init(&project.config.network)?);

    Ok(Network {
        tls: tokio_rustls::TlsConnector::from(Arc::new(tls_config)),
        host_ports: project.config.network.host_ports.clone(),
        tls_passthrough: project.config.network.allowed_hosts_tls.clone(),
        script_engine,
    })
}

pub struct Network {
    tls: tokio_rustls::TlsConnector,
    host_ports: Vec<u16>,
    tls_passthrough: Vec<String>,
    pub(crate) script_engine: std::rc::Rc<ScriptEngine>,
}
