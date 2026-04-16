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

use crate::config::config::DefaultMode;
use crate::network::target::{NetworkTarget, ResolvedTarget};
use crate::project::Project;

/// Build the [`Network`] from the sandbox config: load native CA roots,
/// compile middleware scripts, resolve network targets, and prepare the
/// TLS interceptor with the sandbox's CA.
pub fn setup(project: &Project, container_home: &str) -> anyhow::Result<Network> {
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

    let interceptor = tls::TlsInterceptor::new(&project.ca_cert, &project.ca_key)?;

    let port_forwards: HashMap<u16, u16> =
        rules::port_forwards_from_config(net).into_iter().collect();

    let socket_map: HashMap<String, PathBuf> = net
        .sockets
        .values()
        .filter(|s| s.enabled)
        .map(|s| {
            let guest = crate::util::expand_tilde(&s.guest, std::path::Path::new(container_home))
                .to_string_lossy()
                .into_owned();
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
        default_mode: net.default_mode,
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        allow_targets,
        deny_targets,
        port_forwards,
        socket_map,
    })
}

/// Host-side network proxy state, implementing the `NetworkProxy` RPC
/// interface that the guest supervisor calls for every outbound connection.
pub struct Network {
    default_mode: DefaultMode,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    /// Allow-rule targets with optional middleware.
    allow_targets: Vec<NetworkTarget>,
    /// Deny-rule targets (no middleware; deny wins unconditionally).
    deny_targets: Vec<NetworkTarget>,
    /// Port forward mappings: guest_port → host_port.
    /// Connections to these ports from localhost bypass allow/deny rules.
    port_forwards: HashMap<u16, u16>,
    /// Guest socket path → host socket path mapping for Unix socket forwarding.
    pub(super) socket_map: HashMap<String, PathBuf>,
}

impl Network {
    /// Resolve a host:port to a `ResolvedTarget`.
    ///
    /// Logic:
    /// 0. If this is a port-forwarded localhost connection → always allowed.
    /// 1. If any deny target matches → `allowed = false` immediately.
    /// 2. If any allow target matches → `allowed = true`; middleware collected.
    /// 3. If neither matched → `allowed` follows `default_mode`.
    pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
        // Port-forwarded localhost connections are always allowed.
        if is_localhost_ip(host)
            && let Some(&host_port) = self.port_forwards.get(&port)
        {
            return ResolvedTarget {
                host: "127.0.0.1".to_string(),
                port: host_port,
                middleware: vec![],
                allowed: true,
            };
        }

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

        let allowed = any_allow || matches!(self.default_mode, DefaultMode::Allow);

        ResolvedTarget {
            host: host.to_string(),
            port,
            middleware,
            allowed,
        }
    }
}

fn is_localhost_ip(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1"
}
