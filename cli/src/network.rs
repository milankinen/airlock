mod http;
mod io;
mod matchers;
mod middleware;
pub(crate) mod rules;
mod server;
pub(crate) mod target;
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

    let net = &project.config.network;
    let log = middleware::tracing_log();
    let targets = rules::resolve(net, &log)?;

    let ca_cert = std::fs::read_to_string(&project.ca_cert)?;
    let ca_key = std::fs::read_to_string(&project.ca_key)?;
    let interceptor = tls::TlsInterceptor::new(&ca_cert, &ca_key)?;

    tracing::debug!("network: {} targets resolved", targets.len());

    Ok(Network {
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        targets,
    })
}

pub struct Network {
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    targets: Vec<target::NetworkTarget>,
}
