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
pub(crate) mod grant;
mod host_socket;
pub(crate) mod http;
pub(crate) mod interceptor;
pub(crate) mod io;
mod matchers;
pub(crate) mod middleware;
pub mod reverse_forward;
pub(crate) mod rule_file;
pub(crate) mod rules;
mod server;
pub(crate) mod target;
mod tcp;
#[cfg(test)]
mod tests;
pub(crate) mod tls;
mod traffic;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use tokio::sync::broadcast;

pub use self::control::NetworkControl;
pub use self::deny_reporter::DenyReporter;
use self::grant::GrantAccess;
pub use self::grant::{AgentAccess, GrantContext};
use self::host_socket::HostSocket;
use crate::config::config_values::Policy;
use crate::network::http::middleware::CompiledMiddleware;
use crate::network::interceptor::Interceptor;
use crate::network::target::{
    InjectTarget, InjectedSecret, MiddlewareTarget, NetworkTarget, ResolvedTarget,
};
use crate::sandbox::Project;

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
    ///  - `project_share`: If `true`, the sandbox can write the project
    ///    directory. A host socket path into it must stay in it.
    ///
    /// Returns:
    ///   The network, or error if the config is not valid.
    pub fn new(
        project: &Project,
        container_home: &str,
        tls_client: Arc<rustls::ClientConfig>,
        interceptors: Vec<Rc<dyn Interceptor>>,
        denied_targets: Vec<NetworkTarget>,
        project_share: bool,
    ) -> anyhow::Result<Self> {
        let net = &project.config.network;
        let log = middleware::tracing_log();
        let rule_set = rules::RuleSet::new(net, &project.env)?;
        let middleware_targets = rules::resolve_middleware(net, &project.context.vault, &log)?;

        check_passthrough(net)?;
        check_target_conflicts::check_reverse_forward_conflicts(&labeled_reverse_forwards(net))?;

        let interceptor = tls::TlsInterceptor::new(&project.ca_cert, &project.ca_key)?;

        let port_forwards: HashMap<u16, u16> =
            rules::port_forwards_from_config(net).into_iter().collect();

        let (events, _) = broadcast::channel(NETWORK_EVENTS_BUFFER);

        // A host socket in a writable dir must stay in it, as a mount
        // source.
        let (host_home, host_cwd) = (&project.host_home, &project.host_cwd);
        let mounts = crate::vm::mount::enabled_mounts(&project.config);
        let sources = crate::vm::mount::resolve_sources(&mounts, host_home, host_cwd)?;
        let writable = crate::vm::mount::writable_dirs(project_share.then_some(host_cwd), &sources);
        let mut socket_map = HashMap::new();
        for s in net.sockets.values().filter(|s| s.enabled) {
            let guest = crate::vm::mount::expand_tilde_str(&s.host.target, container_home);
            let host = HostSocket::resolve(&s.host.source, host_home, host_cwd, &writable)?;
            socket_map.insert(guest, Rc::new(host));
        }

        tracing::debug!(
            "network: {} allow, {} deny, {} passthrough, {} middleware, {} inject targets, \
             interceptors [{}], {} denied service targets",
            rule_set.active.allow.len(),
            rule_set.active.deny.len(),
            rule_set.active.passthrough.len(),
            middleware_targets.len(),
            rule_set.inject.len(),
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
            rules: RefCell::new(rule_set),
            middleware_targets,
            interceptors,
            unavailable_targets: denied_targets,
            port_forwards,
            socket_map,
            events,
            next_id: AtomicU64::new(0),
            deny_reporter: DenyReporter::new(),
            public_only: false,
            grants: grant::GrantState::new(&net.grants, net.rule_editing),
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

    /// Let the guest ask for temporary network access. The config
    /// `[network.grants]` can still disable it. Without this call, each
    /// request is denied.
    pub fn with_grants(mut self, context: GrantContext) -> Self {
        self.grants.attach(context);
        self
    }

    /// Get the agent features that this network serves to the sandbox.
    pub fn agent_access(&self) -> AgentAccess {
        self.grants.access()
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
                    label: crate::messages::network::label_service(
                        id.name(),
                        &target.host,
                        target.port.map_or("*".to_string(), |p| p.to_string()),
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
        // On-demand rules count too: an approved command can use them.
        if !rule.is_used() || !rule.passthrough {
            continue;
        }
        for allow in &rule.allow {
            let (host, port) = rules::parse_pattern(allow)?;
            out.push(check_target_conflicts::LabeledTarget {
                label: crate::messages::network::label_rule_passthrough(rule_name, allow),
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
                label: crate::messages::network::label_middleware(mw_name, target_str),
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
        if !rule.is_used() || rule.inject.is_empty() {
            continue;
        }
        for allow in &rule.allow {
            let (host, port) = rules::parse_pattern(allow)?;
            out.push(check_target_conflicts::LabeledTarget {
                label: crate::messages::network::label_rule_inject(rule_name, allow),
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
                label: crate::messages::network::label_ports_guest(
                    group_name,
                    mapping.host,
                    mapping.guest,
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
    /// Compiled network rules. An approved rule change replaces them while
    /// the sandbox runs.
    ///  * Allow targets of the enabled rules
    ///  * Deny targets. Deny wins over allow rules and service hosts. It
    ///    does not apply under `allow-always` or to port forwards.
    ///  * Passthrough subset of the allow targets. Connections that match
    ///    one of these targets get no TLS or HTTP interception. The proxy
    ///    relays the raw bytes.
    ///  * Masked secrets to inject into HTTP headers, with target patterns
    ///  * On-demand rules
    pub(crate) rules: RefCell<rules::RuleSet>,
    /// Compiled middleware with target patterns.
    pub(crate) middleware_targets: Vec<MiddlewareTarget>,
    /// Interceptors of the enabled network services. Each one owns its
    /// targets.
    pub(crate) interceptors: Vec<Rc<dyn Interceptor>>,
    /// Hosts of enabled services that cannot run (no token store).
    /// The network denies them on all ports under every policy. Thus the
    /// agents never sign in without airlock.
    pub(crate) unavailable_targets: Vec<NetworkTarget>,
    /// Port forward mappings: guest_port → host_port.
    pub(crate) port_forwards: HashMap<u16, u16>,
    /// Map from guest socket path to host socket path, for Unix socket
    /// forwarding.
    pub(crate) socket_map: HashMap<String, Rc<HostSocket>>,
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
    /// Settings and state of temporary network grants.
    pub(crate) grants: grant::GrantState,
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
        NetworkControl::new(self.state.clone(), self.public_only)
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

    /// Find how to handle a connection to `host:port` with no grant.
    /// [`Network::resolve_target_in`] has the rules.
    #[cfg(test)]
    pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
        self.resolve_target_in(host, port, None)
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
    /// 4. Deny rules (with the deny patterns of the grant rule): deny.
    /// 5. Host that a network service owns: allow and intercept.
    /// 6. Allow rules and the hosts of the grant: allow with middleware.
    /// 7. No match: `allow-by-default` allows, `deny-by-default` denies.
    ///
    /// Args:
    ///  - `host`, `port`: Destination of the connection
    ///  - `grant`: Access of a temporary grant: host patterns or an
    ///    on-demand rule. They act as more rules (steps 4 and 6). All
    ///    other steps still win. A connection that only the grant allows
    ///    goes to public addresses only.
    ///
    /// Returns:
    ///   The resolved target with the decision and the matching middleware
    ///   and secrets.
    pub(crate) fn resolve_target_in(
        &self,
        host: &str,
        port: u16,
        grant: Option<&GrantAccess>,
    ) -> ResolvedTarget {
        let policy = self.policy();

        // Ad-hoc guest → host forwards do the same check for each connection
        // (see `NetworkControl::allows_port_forward`). Keep the two the same.
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

        let interceptor = if port_forwarded {
            None
        } else {
            self.interceptor_for(host, port)
        };
        // A service owns its hosts on all ports. Deny the ports that no
        // interceptor handles, so that a guest cannot reach the real
        // service on another port and get real tokens.
        let service_hosts = self.service_hosts();
        if !port_forwarded && interceptor.is_none() && is_service_host(&service_hosts, host) {
            return denied(host, port);
        }
        let allowed_without_grant = self.is_allowed(host, port, policy)
            || port_forwarded
            || (interceptor.is_some() && !self.is_denied_by_rule(host, port));
        // The deny patterns of a grant rule act as more deny rules. As the
        // config deny rules, they do not apply under `allow-always` or to
        // port forwards.
        let grant_denies = grant.is_some_and(|g| g.denies(host, port))
            && !matches!(policy, Policy::AllowAlways)
            && !port_forwarded;
        let allowed = !grant_denies
            && (allowed_without_grant
                || grant
                    .is_some_and(|g| g.allows(host, port) && !self.is_denied_by_rule(host, port)));
        // A connection that only the grant allows goes to public addresses
        // only. Then a name that resolves to a local address (DNS rebinding,
        // cloud metadata names) cannot reach the host or the LAN.
        let grant_only = allowed && !allowed_without_grant;
        // Only a deny of the default policy can change with a grant. A deny
        // rule, a local host and the early returns above stay denied.
        let grantable = !allowed
            && !grant_denies
            && matches!(policy, Policy::DenyByDefault)
            && !self.is_denied_by_rule(host, port)
            && grant::is_grantable_host(host);
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
                || (interceptor.is_none()
                    && (self.is_passthrough_target(host, port)
                        || grant.is_some_and(|g| g.passthrough(host, port)))));

        // Secrets are necessary only where the proxy changes headers.
        let secrets = if allowed && !passthrough {
            let mut secrets = self.collect_secrets(host, port);
            if let Some(g) = grant {
                g.add_secrets(host, port, &mut secrets);
            }
            secrets
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
            grantable,
            passthrough,
            public_only: self.public_only || (grant_only && grant::grant_needs_public_only()),
            service_hosts,
        }
    }

    /// Get the canonical hosts of all network services, with the hosts of
    /// the services that cannot run.
    fn service_hosts(&self) -> Rc<[String]> {
        let mut hosts: Vec<String> = Vec::new();
        let targets = self
            .interceptors
            .iter()
            .flat_map(|i| i.targets())
            .chain(&self.unavailable_targets);
        for t in targets {
            let host = matchers::canonical_host(&t.host);
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        hosts.into()
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
        self.rules
            .borrow()
            .active
            .deny
            .iter()
            .any(|t| t.matches(host, port))
    }

    /// Collect the masked secrets of all inject targets that match
    /// `host:port`, with no duplicate variable names.
    fn collect_secrets(&self, host: &str, port: u16) -> Vec<InjectedSecret> {
        let mut out: Vec<InjectedSecret> = Vec::new();
        add_secrets(&self.rules.borrow().inject, host, port, &mut out);
        out
    }

    fn is_passthrough_target(&self, host: &str, port: u16) -> bool {
        self.rules
            .borrow()
            .active
            .passthrough
            .iter()
            .any(|t| t.matches(host, port))
    }

    fn is_allowed(&self, host: &str, port: u16, policy: Policy) -> bool {
        // The allow-always policy overrides deny targets.
        if matches!(policy, Policy::AllowAlways) {
            return true;
        }
        if self.is_denied_by_rule(host, port) {
            return false;
        }
        matches!(policy, Policy::AllowByDefault)
            || self
                .rules
                .borrow()
                .active
                .allow
                .iter()
                .any(|t| t.matches(host, port))
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

/// Add the masked secrets of all `targets` that match `host:port` to
/// `out`, with no duplicate variable names.
pub(crate) fn add_secrets(
    targets: &[InjectTarget],
    host: &str,
    port: u16,
    out: &mut Vec<InjectedSecret>,
) {
    // Each entry is a shared handle. Thus this clones only pointers.
    for target in targets.iter().filter(|t| t.matches(host, port)) {
        for s in &target.secrets {
            if !out.iter().any(|o| o.name == s.name) {
                out.push(s.clone());
            }
        }
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
        grantable: false,
        passthrough: false,
        public_only: false,
        service_hosts: Rc::from([]),
    }
}

/// Check if `host` is one of the canonical `service_hosts`, in any
/// spelling.
pub(crate) fn is_service_host(service_hosts: &[String], host: &str) -> bool {
    let host = matchers::canonical_host(host);
    service_hosts.contains(&host)
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
