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

use crate::config::config::Policy;
use crate::network::http::middleware::CompiledMiddleware;
use crate::network::target::{MiddlewareTarget, NetworkTarget, ResolvedTarget};
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
    let (allow_targets, deny_targets) = rules::resolve(net);
    let middleware_targets = rules::resolve_middleware(net, &log)?;

    let interceptor = tls::TlsInterceptor::new(&project.ca_cert, &project.ca_key)?;

    let port_forwards: HashMap<u16, u16> =
        rules::port_forwards_from_config(net).into_iter().collect();

    let socket_map: HashMap<String, PathBuf> = net
        .sockets
        .values()
        .filter(|s| s.enabled)
        .map(|s| {
            let guest =
                crate::util::expand_tilde(&s.host.target, std::path::Path::new(container_home))
                    .to_string_lossy()
                    .into_owned();
            let host = project.expand_host_tilde(&s.host.source);
            (guest, host)
        })
        .collect();

    tracing::debug!(
        "network: {} allow, {} deny, {} middleware targets",
        allow_targets.len(),
        deny_targets.len(),
        middleware_targets.len()
    );

    Ok(Network {
        policy: net.policy,
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        allow_targets,
        deny_targets,
        middleware_targets,
        port_forwards,
        socket_map,
    })
}

/// Host-side network proxy state, implementing the `NetworkProxy` RPC
/// interface that the guest supervisor calls for every outbound connection.
pub struct Network {
    policy: Policy,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    /// Allow-rule targets.
    allow_targets: Vec<NetworkTarget>,
    /// Deny-rule targets (deny wins unconditionally).
    deny_targets: Vec<NetworkTarget>,
    /// Compiled middleware with target patterns.
    middleware_targets: Vec<MiddlewareTarget>,
    /// Port forward mappings: guest_port → host_port.
    port_forwards: HashMap<u16, u16>,
    /// Guest socket path → host socket path mapping for Unix socket forwarding.
    pub(super) socket_map: HashMap<String, PathBuf>,
}

impl Network {
    /// Resolve a host:port to a `ResolvedTarget`.
    ///
    /// Logic:
    /// 0. `deny-always` → deny immediately.
    /// 1. Localhost port-forward → remap port.
    /// 2. `allow-always` → allow with middleware.
    /// 3. Deny rules → deny wins unconditionally.
    /// 4. Allow rules → allow with middleware.
    /// 5. No match → `allow-by-default` allows, `deny-by-default` denies.
    pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
        // deny-always denies everything.
        if matches!(self.policy, Policy::DenyAlways) {
            return denied(host, port);
        }

        // Localhost port-forward remapping.
        let (host, port, port_forwarded) = if is_localhost(host) {
            if let Some(&host_port) = self.port_forwards.get(&port) {
                ("127.0.0.1", host_port, true)
            } else {
                (host, port, false)
            }
        } else {
            (host, port, false)
        };

        // allow-always skips rules, collects middleware.
        if matches!(self.policy, Policy::AllowAlways) {
            return ResolvedTarget {
                host: host.to_string(),
                port,
                middleware: self.collect_middleware(host, port),
                allowed: true,
            };
        }

        // Deny rules win unconditionally.
        for target in &self.deny_targets {
            if target.matches(host, port) {
                return denied(host, port);
            }
        }

        // Allow rules.
        let allowed = port_forwarded
            || matches!(self.policy, Policy::AllowByDefault)
            || self.allow_targets.iter().any(|t| t.matches(host, port));

        let middleware = if allowed {
            self.collect_middleware(host, port)
        } else {
            vec![]
        };

        ResolvedTarget {
            host: host.to_string(),
            port,
            middleware,
            allowed,
        }
    }

    /// Whether the policy is `deny-always` (blocks everything including sockets).
    pub fn is_deny_always(&self) -> bool {
        matches!(self.policy, Policy::DenyAlways)
    }

    /// Collect compiled middleware from all matching middleware targets.
    fn collect_middleware(&self, host: &str, port: u16) -> Vec<CompiledMiddleware> {
        self.middleware_targets
            .iter()
            .filter(|mt| mt.matches(host, port))
            .map(|mt| mt.middleware.clone())
            .collect()
    }
}

fn denied(host: &str, port: u16) -> ResolvedTarget {
    ResolvedTarget {
        host: host.to_string(),
        port,
        middleware: vec![],
        allowed: false,
    }
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}
