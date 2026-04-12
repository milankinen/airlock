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

use crate::network::target::{NetworkTarget, ResolvedTarget};
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
    let (allow_targets, deny_targets) = rules::resolve(net, &log)?;

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

    tracing::debug!(
        "network: {} allow targets, {} deny targets",
        allow_targets.len(),
        deny_targets.len()
    );

    Ok(Network {
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        allow_targets,
        deny_targets,
        socket_map,
    })
}

/// Host-side network proxy state, implementing the `NetworkProxy` RPC
/// interface that the guest supervisor calls for every outbound connection.
pub struct Network {
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    /// Allow-rule targets with optional middleware.
    allow_targets: Vec<NetworkTarget>,
    /// Deny-rule targets (no middleware; deny wins unconditionally).
    deny_targets: Vec<NetworkTarget>,
    /// Guest socket path → host socket path mapping for Unix socket forwarding.
    pub(super) socket_map: HashMap<String, PathBuf>,
}

impl Network {
    /// Resolve a host:port to a `ResolvedTarget`.
    ///
    /// Logic:
    /// 1. If any deny target matches → `allowed = false` immediately.
    /// 2. If no allow target matches → `allowed = false`.
    /// 3. Otherwise → `allowed = true`; middleware collected from all matching allow rules.
    pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
        // Deny wins: checked first, no middleware involved.
        for target in &self.deny_targets {
            if target.matches(host, port) {
                return ResolvedTarget {
                    host: host.to_string(),
                    port,
                    middleware: vec![],
                    allowed: false,
                };
            }
        }

        // Collect middleware from all matching allow rules.
        let mut middleware = vec![];
        let mut any_allow = false;
        for target in &self.allow_targets {
            if target.matches(host, port) {
                any_allow = true;
                middleware.extend(target.middleware.iter().cloned());
            }
        }

        ResolvedTarget {
            host: host.to_string(),
            port,
            middleware,
            allowed: any_allow,
        }
    }
}
