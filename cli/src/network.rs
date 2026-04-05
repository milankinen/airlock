mod http;
mod io;
mod matchers;
mod middleware;
mod server;
mod tcp;
#[cfg(test)]
mod tests;
mod tls;

use std::rc::Rc;
use std::sync::Arc;

use crate::project::Project;

pub fn setup(project: &Project) -> anyhow::Result<Network> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("native certs") {
        let _ = root_store.add(cert);
    }

    let tls_client = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let mw = middleware::Middleware::init(&project.config.network)?;

    let ca_cert = std::fs::read_to_string(&project.ca_cert)?;
    let ca_key = std::fs::read_to_string(&project.ca_key)?;
    let interceptor = tls::TlsInterceptor::new(&ca_cert, &ca_key)?;

    Ok(Network {
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        host_ports: project.config.network.host_ports.clone(),
        tls_passthrough: project.config.network.tls_passthrough.clone(),
        middleware: Rc::new(mw),
    })
}

pub struct Network {
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    host_ports: Vec<u16>,
    tls_passthrough: Vec<String>,
    middleware: Rc<middleware::Middleware>,
}
