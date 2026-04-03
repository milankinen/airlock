mod server;
pub mod scripting;

use crate::project::Project;
use scripting::ScriptEngine;
use std::sync::Arc;

pub fn setup(project: &Project) -> anyhow::Result<Network> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("native certs") {
        let _ = root_store.add(cert);
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let script_engine = ScriptEngine::init(
        &project.config.network,
    )?;

    Ok(Network {
        tls: tokio_rustls::TlsConnector::from(Arc::new(tls_config)),
        host_ports: project.config.network.host_ports.clone(),
        script_engine,
    })
}

pub struct Network {
    tls: tokio_rustls::TlsConnector,
    host_ports: Vec<u16>,
    script_engine: ScriptEngine,
}
