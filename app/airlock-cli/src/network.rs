//! Host-side network proxy for the sandbox.
//!
//! Decides for each outgoing sandbox connection if it is allowed, and how to
//! relay it to the upstream server. The proxy can also:
//!  * change the network policy while the sandbox runs
//!  * send monitor events about connections and traffic
//!  * tell the guest about denied connections

mod check_target_conflicts;
mod control;
mod deny_reporter;
pub(crate) mod http;
pub(crate) mod interceptor;
pub(crate) mod io;
mod matchers;
pub(crate) mod middleware;
pub mod reverse_forward;
pub(crate) mod rules;
mod server;
pub(crate) mod target;
mod tcp;
#[cfg(test)]
mod tests;
pub(crate) mod tls;
mod traffic;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use tokio::sync::broadcast;

pub use self::control::NetworkControl;
pub use self::deny_reporter::DenyReporter;
use crate::config::config_values::Policy;
use crate::network::http::middleware::CompiledMiddleware;
use crate::network::interceptor::Interceptor;
use crate::network::target::{
    InjectTarget, InjectedSecret, MiddlewareTarget, NetworkTarget, ResolvedTarget,
};
use crate::project::Project;

const NETWORK_EVENTS_BUFFER: usize = 10;

/// Make the TLS client config for upstream connections.
/// The config trusts the native CA roots of the host. The proxy and the
/// network services use it.
pub fn native_tls_client() -> Arc<rustls::ClientConfig> {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("native certs") {
        let _ = root_store.add(cert);
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth(),
    )
}

impl Network {
    /// Make the network from the sandbox config.
    /// Args:
    ///  - `project`: Project with the network config, the vault and the
    ///    sandbox CA
    ///  - `container_home`: Home directory in the guest, for `~` expansion
    ///    of guest socket paths
    ///  - `tls_client`: TLS client config for upstream connections
    ///  - `interceptors`: Network service interceptors. Each one handles the
    ///    hosts that it owns.
    ///  - `denied_targets`: Targets to deny under every policy (the hosts of
    ///    services that cannot run).
    ///
    /// Returns:
    ///   The network, or error if the config is not valid.
    pub fn new(
        project: &Project,
        container_home: &str,
        tls_client: Arc<rustls::ClientConfig>,
        interceptors: Vec<Rc<dyn Interceptor>>,
        denied_targets: Vec<NetworkTarget>,
    ) -> anyhow::Result<Self> {
        let net = &project.config.network;
        let log = middleware::tracing_log();
        let rule_targets = rules::resolve(net)?;
        let middleware_targets = rules::resolve_middleware(net, &project.context.vault, &log)?;
        let inject_targets = rules::resolve_inject(net, &project.env)?;

        check_passthrough(net)?;
        check_target_conflicts::check_reverse_forward_conflicts(&labeled_reverse_forwards(net))?;

        let interceptor = tls::TlsInterceptor::new(&project.ca_cert, &project.ca_key)?;

        let port_forwards: HashMap<u16, u16> =
            rules::port_forwards_from_config(net).into_iter().collect();

        let (events, _) = broadcast::channel(NETWORK_EVENTS_BUFFER);

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
            "network: {} allow, {} deny, {} passthrough, {} middleware, {} inject targets, \
             interceptors [{}], {} denied service targets",
            rule_targets.allow.len(),
            rule_targets.deny.len(),
            rule_targets.passthrough.len(),
            middleware_targets.len(),
            inject_targets.len(),
            interceptors
                .iter()
                .map(|i| i.name())
                .collect::<Vec<_>>()
                .join(", "),
            denied_targets.len(),
        );

        Ok(Network {
            state: Arc::new(RwLock::new(NetworkState { policy: net.policy })),
            tls_client,
            interceptor: Rc::new(interceptor),
            allow_targets: rule_targets.allow,
            deny_targets: rule_targets.deny,
            passthrough_targets: rule_targets.passthrough,
            middleware_targets,
            inject_targets,
            interceptors,
            unavailable_targets: denied_targets,
            port_forwards,
            socket_map,
            events,
            next_id: AtomicU64::new(0),
            deny_reporter: DenyReporter::new(),
            public_only: false,
        })
    }

    /// Allow connections to public addresses only (used for the install
    /// boot). The network denies loopback, private, link-local and other
    /// local destinations of the host. It checks the name, the IP literal
    /// and the resolved addresses (see [`target::is_public_ip`]).
    pub fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }
}

/// Make sure that no passthrough target is also intercepted.
/// Middleware, inject and the hosts of enabled network services all need
/// interception. Thus all of them conflict with passthrough.
/// Returns:
///   Error that names the conflicting targets, if a conflict exists.
pub(crate) fn check_passthrough(net: &crate::config::config_values::Network) -> anyhow::Result<()> {
    let mut intercepting = labeled_middleware(net)?;
    intercepting.extend(labeled_inject(net)?);
    intercepting.extend(labeled_services(net));
    check_target_conflicts::check_passthrough_conflicts(&labeled_passthrough(net)?, &intercepting)
}

/// Get the hosts of the enabled network services, with labels for the
/// passthrough conflict check.
fn labeled_services(
    net: &crate::config::config_values::Network,
) -> Vec<check_target_conflicts::LabeledTarget> {
    crate::services::enabled(&net.services)
        .into_iter()
        .flat_map(|id| {
            id.targets()
                .into_iter()
                .map(move |target| check_target_conflicts::LabeledTarget {
                    label: format!(
                        "service `{}` host `{}:{}`",
                        id.name(),
                        target.host,
                        target.port.map_or("*".to_string(), |p| p.to_string())
                    ),
                    target,
                })
        })
        .collect()
}

/// Get the passthrough targets from the config, with labels for the
/// conflict check.
///
/// The validator does not parse the label. Thus the label format is set
/// here, where the config shape is known.
fn labeled_passthrough(
    net: &crate::config::config_values::Network,
) -> anyhow::Result<Vec<check_target_conflicts::LabeledTarget>> {
    let mut out = Vec::new();
    for (rule_name, rule) in &net.rules {
        if !rule.enabled || !rule.passthrough {
            continue;
        }
        for allow in &rule.allow {
            let (host, port) = rules::parse_pattern(allow)?;
            out.push(check_target_conflicts::LabeledTarget {
                label: format!("rule `{rule_name}` allow=`{allow}` (passthrough)"),
                target: NetworkTarget {
                    host: host.to_string(),
                    port,
                },
            });
        }
    }
    Ok(out)
}

/// Get the middleware targets from the config, with labels for the
/// conflict check. See also [`labeled_passthrough`].
fn labeled_middleware(
    net: &crate::config::config_values::Network,
) -> anyhow::Result<Vec<check_target_conflicts::LabeledTarget>> {
    let mut out = Vec::new();
    for (mw_name, mw) in &net.middleware {
        if !mw.enabled {
            continue;
        }
        for target_str in &mw.target {
            let (host, port) = rules::parse_pattern(target_str)?;
            out.push(check_target_conflicts::LabeledTarget {
                label: format!("middleware `{mw_name}` target=`{target_str}`"),
                target: NetworkTarget {
                    host: host.to_string(),
                    port,
                },
            });
        }
    }
    Ok(out)
}

/// Get the inject targets from the config, with labels for the
/// passthrough conflict check. Inject targets are the allow patterns of
/// rules that have a non-empty `inject` list. See also
/// [`labeled_passthrough`] and [`labeled_middleware`].
fn labeled_inject(
    net: &crate::config::config_values::Network,
) -> anyhow::Result<Vec<check_target_conflicts::LabeledTarget>> {
    let mut out = Vec::new();
    for (rule_name, rule) in &net.rules {
        if !rule.enabled || rule.inject.is_empty() {
            continue;
        }
        for allow in &rule.allow {
            let (host, port) = rules::parse_pattern(allow)?;
            out.push(check_target_conflicts::LabeledTarget {
                label: format!("rule `{rule_name}` inject target=`{allow}`"),
                target: NetworkTarget {
                    host: host.to_string(),
                    port,
                },
            });
        }
    }
    Ok(out)
}

/// Get the reverse port forwards (`.guest` entries) from the config, with
/// labels for the host port conflict check.
fn labeled_reverse_forwards(
    net: &crate::config::config_values::Network,
) -> Vec<check_target_conflicts::LabeledReverseForward> {
    let mut out = Vec::new();
    for (group_name, pf) in &net.ports {
        if !pf.enabled {
            continue;
        }
        for mapping in &pf.guest {
            out.push(check_target_conflicts::LabeledReverseForward {
                label: format!(
                    "ports `{group_name}` guest=`{}:{}`",
                    mapping.host, mapping.guest
                ),
                host_port: mapping.host,
            });
        }
    }
    out
}

/// Mutable runtime state that the network task and the TUI share.
///
/// Keep this state small. The TUI accesses it through [`NetworkControl`].
/// The network task accesses it through [`Network::policy`] and similar
/// functions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NetworkState {
    /// Current top-level network policy.
    pub policy: Policy,
}

/// Cloneable view of a served [`Network`].
/// Gives live policy control and the monitor event stream. The VM keeps the
/// handle and gives it to the runtime.
#[derive(Clone)]
pub struct NetworkHandle {
    control: NetworkControl,
    events: broadcast::Sender<airlock_monitor::NetworkEvent>,
}

impl NetworkHandle {
    /// Get a thread-safe handle for live changes to the network state.
    /// The TUI uses it.
    pub fn control(&self) -> NetworkControl {
        self.control.clone()
    }

    /// Subscribe to network events.
    pub fn events(&self) -> broadcast::Receiver<airlock_monitor::NetworkEvent> {
        self.events.subscribe()
    }
}

/// Host-side network proxy.
///
/// When a guest process opens a TCP connection, the guest supervisor sends
/// it to the host through the `NetworkProxy` RPC interface. This type
/// implements that interface. It decides if the connection is allowed, if
/// TLS is intercepted, and how traffic goes to the upstream server.
pub struct Network {
    /// Mutable runtime state. The hot path uses `parking_lot::RwLock` reads
    /// (no contention, no poisoning). The TUI keeps a clone of this `Arc`
    /// in [`NetworkControl`] to change the policy live.
    pub(crate) state: Arc<RwLock<NetworkState>>,
    pub(crate) tls_client: Arc<rustls::ClientConfig>,
    pub(crate) interceptor: Rc<tls::TlsInterceptor>,
    /// Allow-rule targets.
    pub(crate) allow_targets: Vec<NetworkTarget>,
    /// Deny-rule targets. Deny wins over allow rules and service hosts.
    /// It does not apply under `allow-always` or to port forwards.
    pub(crate) deny_targets: Vec<NetworkTarget>,
    /// Passthrough subset of `allow_targets`. Connections that match one of
    /// these targets get no TLS or HTTP interception. The proxy relays the
    /// raw bytes.
    pub(crate) passthrough_targets: Vec<NetworkTarget>,
    /// Compiled middleware with target patterns.
    pub(crate) middleware_targets: Vec<MiddlewareTarget>,
    /// Masked secrets to inject into HTTP headers, with target patterns.
    pub(crate) inject_targets: Vec<InjectTarget>,
    /// Interceptors of the enabled network services. Each one owns its
    /// targets.
    pub(crate) interceptors: Vec<Rc<dyn Interceptor>>,
    /// Hosts of enabled services that cannot run (no token store).
    /// The network denies them under every policy. Thus the agents never
    /// sign in without airlock.
    pub(crate) unavailable_targets: Vec<NetworkTarget>,
    /// Port forward mappings: guest_port → host_port.
    pub(crate) port_forwards: HashMap<u16, u16>,
    /// Map from guest socket path to host socket path, for Unix socket
    /// forwarding.
    pub(crate) socket_map: HashMap<String, PathBuf>,
    /// Sender of network events for subscribers.
    // TODO: this NetworkEvent should be Network event agnostic to any TUI
    pub(crate) events: broadcast::Sender<airlock_monitor::NetworkEvent>,
    /// Monotonic counter for connection ids. The TUI uses the ids to pair
    /// each `Disconnect` event with its `Connect` event.
    pub(crate) next_id: AtomicU64,
    /// Host-to-guest notifier for each denied connection. It does nothing
    /// until the supervisor handshake completes.
    pub(crate) deny_reporter: Rc<DenyReporter>,
    /// If true, connect to public addresses only
    /// (see [`Network::public_only`]).
    pub(crate) public_only: bool,
}

impl Network {
    /// Get the deny notifier. The caller attaches it to the supervisor
    /// when the vsock handshake completes. See [`DenyReporter::attach`].
    pub fn deny_reporter(&self) -> Rc<DenyReporter> {
        self.deny_reporter.clone()
    }

    /// Get a thread-safe handle to change the runtime state.
    /// The TUI gets it through [`NetworkHandle`]. The TUI uses it to change
    /// the policy without access to the `Network` internals.
    fn control(&self) -> NetworkControl {
        NetworkControl::new(self.state.clone())
    }

    /// Get the parts of the network that stay available after the network
    /// is served: the live control and the event stream. Call this before
    /// [`crate::rpc::serve_network`] consumes the network.
    pub fn handle(&self) -> NetworkHandle {
        NetworkHandle {
            control: self.control(),
            events: self.events.clone(),
        }
    }

    /// Get the current top-level policy. The hot connect path calls this.
    pub fn policy(&self) -> Policy {
        // One uncontended `RwLock` read.
        self.state.read().policy
    }

    /// Find how to handle a connection to `host:port`.
    ///
    /// The rules, in order:
    /// 0. `deny-always`: deny. A public-only network denies local names and
    ///    non-public IP literals. It also checks the resolved addresses when
    ///    it connects.
    /// 1. Localhost port forward: change the port, and allow with
    ///    passthrough.
    /// 2. Host of an enabled service that cannot run: deny under every
    ///    policy.
    /// 3. `allow-always`: allow with middleware.
    /// 4. Deny rules: deny.
    /// 5. Host that a network service owns: allow and intercept.
    /// 6. Allow rules: allow with middleware.
    /// 7. No match: `allow-by-default` allows, `deny-by-default` denies.
    ///
    /// Returns:
    ///   The resolved target with the decision and the matching middleware
    ///   and secrets.
    pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
        let policy = self.policy();

        if matches!(policy, Policy::DenyAlways) || (self.public_only && is_local_host(host)) {
            return denied(host, port);
        }

        // Localhost port forward: change the destination port.
        let (host, port, port_forwarded) = if is_localhost(host) {
            if let Some(&host_port) = self.port_forwards.get(&port) {
                ("127.0.0.1", host_port, true)
            } else {
                (host, port, false)
            }
        } else {
            (host, port, false)
        };

        if !port_forwarded
            && self
                .unavailable_targets
                .iter()
                .any(|t| t.matches(host, port))
        {
            return denied(host, port);
        }
        let interceptor = if port_forwarded {
            None
        } else {
            self.interceptor_for(host, port)
        };
        let allowed = self.is_allowed(host, port, policy)
            || port_forwarded
            || (interceptor.is_some() && !self.is_denied_by_rule(host, port));
        let middleware = if allowed {
            self.collect_middleware(host, port)
        } else {
            vec![]
        };

        // Port-forwarded destinations always use passthrough. The guest can
        // use any protocol on localhost, not only HTTP. Interception would
        // break non-HTTP forwards (for example Postgres).
        // A service host never uses passthrough. The service must see its
        // requests.
        let passthrough = allowed
            && (port_forwarded
                || (interceptor.is_none() && self.is_passthrough_target(host, port)));

        // Secrets are necessary only where the proxy changes headers.
        let secrets = if allowed && !passthrough {
            self.collect_secrets(host, port)
        } else {
            vec![]
        };

        ResolvedTarget {
            host: host.to_string(),
            port,
            middleware,
            secrets,
            interceptor: interceptor.filter(|_| allowed),
            allowed,
            passthrough,
            public_only: self.public_only,
        }
    }

    /// Find the enabled service interceptor that owns `host:port`.
    /// Interceptors own disjoint hosts. Thus the first match is the only one.
    fn interceptor_for(&self, host: &str, port: u16) -> Option<Rc<dyn Interceptor>> {
        self.interceptors
            .iter()
            .find(|s| s.targets().iter().any(|t| t.matches(host, port)))
            .cloned()
    }

    fn is_denied_by_rule(&self, host: &str, port: u16) -> bool {
        self.deny_targets.iter().any(|t| t.matches(host, port))
    }

    /// Collect the masked secrets of all inject targets that match
    /// `host:port`, with no duplicate variable names.
    fn collect_secrets(&self, host: &str, port: u16) -> Vec<InjectedSecret> {
        // Each entry is a shared handle. Thus this clones only pointers.
        let mut out: Vec<InjectedSecret> = Vec::new();
        for target in self.inject_targets.iter().filter(|t| t.matches(host, port)) {
            for s in &target.secrets {
                if !out.iter().any(|o| o.name == s.name) {
                    out.push(s.clone());
                }
            }
        }
        out
    }

    fn is_passthrough_target(&self, host: &str, port: u16) -> bool {
        self.passthrough_targets
            .iter()
            .any(|t| t.matches(host, port))
    }

    fn is_allowed(&self, host: &str, port: u16, policy: Policy) -> bool {
        // The allow-always policy overrides deny targets.
        if matches!(policy, Policy::AllowAlways) {
            return true;
        }
        // Deny rules always win.
        if self.is_denied_by_rule(host, port) {
            return false;
        }
        matches!(policy, Policy::AllowByDefault)
            || self.allow_targets.iter().any(|t| t.matches(host, port))
    }

    /// Return true if the policy is `deny-always`. This policy blocks all
    /// traffic, also sockets.
    pub fn is_deny_always(&self) -> bool {
        matches!(self.policy(), Policy::DenyAlways)
    }

    /// Get a new monotonic id for a new connection.
    pub fn next_connection_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a `Connect` event to all subscribers.
    /// Args:
    ///  - `id`: Connection id from [`Network::next_connection_id`]
    ///  - `host`, `port`: Destination of the connection
    ///  - `allowed`: True if the network allowed the connection.
    pub fn emit_connect(&self, id: u64, host: &str, port: u16, allowed: bool) {
        // Usually there are no subscribers (runs without the monitor).
        // Return before the string clones in that case.
        if self.events.receiver_count() == 0 {
            return;
        }
        let info = airlock_monitor::ConnectInfo {
            id,
            timestamp: std::time::SystemTime::now(),
            host: host.to_string(),
            port,
            allowed,
        };
        let _ = self
            .events
            .send(airlock_monitor::NetworkEvent::Connect(Arc::new(info)));
    }

    /// Send a `Disconnect` event for an earlier [`Network::emit_connect`].
    pub fn emit_disconnect(&self, id: u64) {
        if self.events.receiver_count() == 0 {
            return;
        }
        let info = airlock_monitor::DisconnectInfo {
            id,
            timestamp: std::time::SystemTime::now(),
        };
        let _ = self
            .events
            .send(airlock_monitor::NetworkEvent::Disconnect(Arc::new(info)));
    }

    /// Collect the compiled middleware of all matching middleware targets.
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
        secrets: vec![],
        interceptor: None,
        allowed: false,
        passthrough: false,
        public_only: false,
    }
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Return true if `host` is a non-public destination without a DNS lookup:
/// a `localhost` name or a non-public IP literal.
fn is_local_host(host: &str) -> bool {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || target::ip_literal(host).is_some_and(|ip| !target::is_public_ip(ip))
}
