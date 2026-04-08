//! Network proxy layer — the host-side counterpart of the guest's transparent
//! TCP proxy.
//!
//! When the guest process opens a TCP connection, the supervisor forwards it
//! via RPC to this module. The host decides whether to allow the connection
//! (based on config rules), whether to intercept TLS (for HTTP middleware),
//! and how to relay traffic to the real server.

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

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::network::target::ResolvedTarget;
use crate::project::Project;

/// Build the [`Network`] from the project config: load native CA roots,
/// compile middleware scripts, resolve network targets, and prepare the
/// TLS interceptor with the project's CA.
pub fn setup(project: &Project, bundle: &crate::oci::Bundle) -> anyhow::Result<Network> {
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

    let socket_map: HashMap<String, PathBuf> = net
        .sockets
        .values()
        .filter(|s| s.enabled)
        .map(|s| {
            let guest = bundle.expand_tilde(&s.guest).to_string_lossy().into_owned();
            let host = project.expand_host_tilde(&s.host);
            (guest, host)
        })
        .collect();

    tracing::debug!("network: {} targets resolved", targets.len());

    Ok(Network {
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        targets,
        socket_map,
    })
}

/// Host-side network proxy state, implementing the `NetworkProxy` RPC
/// interface that the guest supervisor calls for every outbound connection.
pub struct Network {
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    targets: Vec<target::NetworkTarget>,
    /// Guest socket path → host socket path mapping for Unix socket forwarding.
    pub(super) socket_map: HashMap<String, PathBuf>,
}

impl Network {
    pub fn resolve_target(&self, host: &str, port: u16) -> Option<ResolvedTarget> {
        let mut matches = false;
        let mut resolved = ResolvedTarget {
            host: host.to_string(),
            port,
            http_only: false,
            middleware: vec![],
        };
        for target in &self.targets {
            if target.matches(host, port) {
                matches = true;
                resolved.http_only = resolved.http_only || target.http_only;
                resolved.middleware.extend(target.middleware.clone());
            }
        }
        if matches { Some(resolved) } else { None }
    }
}
