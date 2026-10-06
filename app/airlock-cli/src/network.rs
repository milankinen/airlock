//! Network proxy layer — the host-side counterpart of the guest's transparent
//! TCP proxy.
//!
//! When the guest process opens a TCP connection, the supervisor forwards it
//! via RPC to this module. The host decides whether to allow the connection
//! (based on config rules), whether to intercept TLS (for HTTP middleware),
//! and how to relay traffic to the real server.

mod check_target_conflicts;
mod control;
mod deny_reporter;
pub(crate) mod http;
pub(crate) mod interceptor;
pub(crate) mod io;
mod matchers;
mod middleware;
pub mod reverse_forward;
pub(crate) mod rules;
mod server;
pub(crate) mod target;
mod tcp;
#[cfg(test)]
mod tests;
mod tls;
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

/// The TLS client config of the proxy's upstream connections: the host's
/// native CA roots. The network services connect upstream with it too.
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
    /// Build the network from the sandbox config: compile middleware
    /// scripts, resolve network and inject targets, and prepare the TLS
    /// interceptor with the sandbox's CA. `interceptors` handle the hosts
    /// they own (the network services); `denied_targets` are denied under
    /// every policy (the hosts of services that cannot run).
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

    /// Reach public addresses only (the install boot): deny the host's
    /// loopback, private, link-local and other local destinations, by
    /// name, by IP literal and by the addresses a name resolves to (see
    /// [`target::is_public_ip`]).
    pub fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }
}

/// Check that no passthrough target is also intercepted. Inject needs
/// interception just like middleware, and so do the hosts of the enabled
/// network services, so they conflict with passthrough the same way.
pub(crate) fn check_passthrough(net: &crate::config::config_values::Network) -> anyhow::Result<()> {
    let mut intercepting = labeled_middleware(net)?;
    intercepting.extend(labeled_inject(net)?);
    intercepting.extend(labeled_services(net));
    check_target_conflicts::check_passthrough_conflicts(&labeled_passthrough(net)?, &intercepting)
}

/// The hosts of the enabled network services, labeled for passthrough
/// conflict checking.
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

/// Extract labeled passthrough targets from the config for conflict
/// checking. Each entry carries a display label — the validator treats
/// that label as opaque, so formatting lives here where the config shape
/// is known.
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

/// Extract labeled middleware targets from the config for conflict
/// checking. Paired with [`labeled_passthrough`].
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

/// Extract labeled inject targets (allow patterns of rules with a non-empty
/// `inject` list) for passthrough conflict checking. Paired with
/// [`labeled_passthrough`], like [`labeled_middleware`].
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

/// Extract labeled reverse port forwards (`.guest` entries) from the
/// config for host-port conflict checking.
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

/// Mutable runtime state shared between the network task and the TUI.
/// Kept intentionally small: the TUI reaches in via [`NetworkControl`], the
/// network task reaches in via [`Network::policy`] and friends.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NetworkState {
    pub policy: Policy,
}

/// Cloneable view of a served [`Network`]: live policy control plus the
/// monitor event stream. Held by the VM and handed to the runtime.
#[derive(Clone)]
pub struct NetworkHandle {
    control: NetworkControl,
    events: broadcast::Sender<airlock_monitor::NetworkEvent>,
}

impl NetworkHandle {
    /// Thread-safe handle for live network-state edits (the TUI).
    pub fn control(&self) -> NetworkControl {
        self.control.clone()
    }

    /// Subscribe to network events.
    pub fn events(&self) -> broadcast::Receiver<airlock_monitor::NetworkEvent> {
        self.events.subscribe()
    }
}

/// Host-side network proxy state, implementing the `NetworkProxy` RPC
/// interface that the guest supervisor calls for every outbound connection.
pub struct Network {
    /// Mutable runtime state. Reads on the hot path use `parking_lot::RwLock`
    /// reads (no contention, no poisoning). The TUI holds a clone of this
    /// `Arc` through [`NetworkControl`] to mutate policy live.
    state: Arc<RwLock<NetworkState>>,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    /// Allow-rule targets.
    allow_targets: Vec<NetworkTarget>,
    /// Deny-rule targets (deny wins unconditionally).
    deny_targets: Vec<NetworkTarget>,
    /// Passthrough subset of `allow_targets` — connections matching any of
    /// these skip TLS/HTTP interception entirely and are relayed raw.
    passthrough_targets: Vec<NetworkTarget>,
    /// Compiled middleware with target patterns.
    middleware_targets: Vec<MiddlewareTarget>,
    /// Masked secrets to inject into HTTP headers, with target patterns.
    inject_targets: Vec<InjectTarget>,
    /// The enabled network services' interceptors; each owns its targets.
    interceptors: Vec<Rc<dyn Interceptor>>,
    /// The hosts of enabled services that cannot run (no token store):
    /// denied under every policy, so the agents never sign in without
    /// airlock.
    unavailable_targets: Vec<NetworkTarget>,
    /// Port forward mappings: guest_port → host_port.
    port_forwards: HashMap<u16, u16>,
    /// Guest socket path → host socket path mapping for Unix socket forwarding.
    pub(crate) socket_map: HashMap<String, PathBuf>,
    /// Network events to subscribe to
    // TODO: this NetworkEvent should be Network event agnostic to any TUI
    events: broadcast::Sender<airlock_monitor::NetworkEvent>,
    /// Monotonic counter for connection ids. Used by the TUI to pair
    /// `Disconnect` events with their originating `Connect`.
    next_id: AtomicU64,
    /// Host → guest notifier for every denied connection. Populated once
    /// the supervisor handshake completes; no-op until then.
    pub(super) deny_reporter: Rc<DenyReporter>,
    /// Connect to public addresses only ([`Network::public_only`]).
    public_only: bool,
}

impl Network {
    /// Deny notifier, attached to the supervisor once the vsock handshake
    /// completes. See [`DenyReporter::attach`].
    pub fn deny_reporter(&self) -> Rc<DenyReporter> {
        self.deny_reporter.clone()
    }

    /// Return a thread-safe handle for mutating runtime state. Handed to the
    /// TUI (through [`NetworkHandle`]); the TUI uses it to flip policy /
    /// toggle rules without touching `Network` internals.
    fn control(&self) -> NetworkControl {
        NetworkControl::new(self.state.clone())
    }

    /// The parts of the network that outlive it being served: the live
    /// control and the event stream. Take it before [`crate::rpc::serve_network`]
    /// consumes the network.
    pub fn handle(&self) -> NetworkHandle {
        NetworkHandle {
            control: self.control(),
            events: self.events.clone(),
        }
    }

    /// Current top-level policy. Called on the hot connect path — one
    /// uncontended `RwLock` read.
    pub fn policy(&self) -> Policy {
        self.state.read().policy
    }

    /// Resolve a host:port to a `ResolvedTarget`.
    ///
    /// Logic:
    /// 0. `deny-always` → deny immediately; a public-only network denies
    ///    local names and non-public IP literals (and checks the resolved
    ///    addresses on connect).
    /// 1. Localhost port-forward → remap port.
    /// 2. `allow-always` → allow with middleware.
    /// 3. Deny rules → deny wins unconditionally.
    /// 4. A host owned by a network service → allow, intercepted; a host
    ///    of an enabled service that cannot run → deny (under every
    ///    policy).
    /// 5. Allow rules → allow with middleware.
    /// 6. No match → `allow-by-default` allows, `deny-by-default` denies.
    pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
        let policy = self.policy();

        // deny-always denies everything.
        if matches!(policy, Policy::DenyAlways) || (self.public_only && is_local_host(host)) {
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

        // Port-forwarded destinations always passthrough — the guest side
        // is talking to an arbitrary protocol on localhost, not necessarily
        // HTTP; intercepting would break non-HTTP forwards (e.g. Postgres).
        // A service host never does: the service must see its requests.
        let passthrough = allowed
            && (port_forwarded
                || (interceptor.is_none() && self.is_passthrough_target(host, port)));

        // Secrets only matter where headers are actually rewritten.
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

    /// The enabled service interceptor that owns `host:port`. Interceptors
    /// own disjoint hosts, so the first match is the only one.
    fn interceptor_for(&self, host: &str, port: u16) -> Option<Rc<dyn Interceptor>> {
        self.interceptors
            .iter()
            .find(|s| s.targets().iter().any(|t| t.matches(host, port)))
            .cloned()
    }

    fn is_denied_by_rule(&self, host: &str, port: u16) -> bool {
        self.deny_targets.iter().any(|t| t.matches(host, port))
    }

    /// Collect the masked secrets of every inject target matching
    /// `host:port`, deduplicated by variable name. Each entry is a shared
    /// handle, so this only clones pointers.
    fn collect_secrets(&self, host: &str, port: u16) -> Vec<InjectedSecret> {
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
        // always-allow policy overrules deny targets
        if matches!(policy, Policy::AllowAlways) {
            return true;
        }
        // Deny rules win unconditionally.
        if self.is_denied_by_rule(host, port) {
            return false;
        }
        matches!(policy, Policy::AllowByDefault)
            || self.allow_targets.iter().any(|t| t.matches(host, port))
    }

    /// Whether the policy is `deny-always` (blocks everything including sockets).
    pub fn is_deny_always(&self) -> bool {
        matches!(self.policy(), Policy::DenyAlways)
    }

    /// Allocate a fresh monotonic id for a new connection.
    pub fn next_connection_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Broadcast a `Connect` event. Silently drops when there are no
    /// subscribers (common case on non-monitor runs) — and short-circuits
    /// before any string cloning in that case.
    pub fn emit_connect(&self, id: u64, host: &str, port: u16, allowed: bool) {
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

    /// Broadcast a `Disconnect` event matching a prior `emit_connect`.
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

/// Whether `host` names a non-public destination without DNS: a
/// `localhost` name or a non-public IP literal.
fn is_local_host(host: &str) -> bool {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || target::ip_literal(host).is_some_and(|ip| !target::is_public_ip(ip))
}

#[cfg(test)]
mod labeled_target_tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::config_values::{self, MiddlewareRule, NetworkRule};

    fn rule(allow: &[&str], passthrough: bool, enabled: bool) -> NetworkRule {
        NetworkRule {
            enabled,
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: vec![],
            passthrough,
            inject: vec![],
        }
    }

    fn mw(target: &[&str], enabled: bool) -> MiddlewareRule {
        MiddlewareRule {
            enabled,
            target: target.iter().map(|s| (*s).to_string()).collect(),
            env: BTreeMap::new(),
            script: "function on_request(req) return req end".to_string(),
        }
    }

    fn net(
        rules: Vec<(&str, NetworkRule)>,
        middleware: Vec<(&str, MiddlewareRule)>,
    ) -> config_values::Network {
        config_values::Network {
            policy: Policy::DenyByDefault,
            rules: rules.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            middleware: middleware
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
            services: BTreeMap::default(),
        }
    }

    #[test]
    fn labeled_passthrough_skips_non_passthrough_and_disabled() {
        let n = net(
            vec![
                ("pt-on", rule(&["a:1"], true, true)),
                ("pt-off", rule(&["b:2"], true, false)),
                ("plain", rule(&["c:3"], false, true)),
            ],
            vec![],
        );
        let got: Vec<String> = labeled_passthrough(&n)
            .unwrap()
            .into_iter()
            .map(|lt| lt.label)
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("pt-on"), "got: {got:?}");
    }

    #[test]
    fn labeled_inject_lists_allow_patterns_of_injecting_rules() {
        let mut injecting = rule(&["a:1", "b:2"], false, true);
        injecting.inject = vec!["TOKEN".to_string()];
        let mut disabled = rule(&["c:3"], false, false);
        disabled.inject = vec!["TOKEN".to_string()];
        let n = net(
            vec![
                ("inj", injecting),
                ("inj-off", disabled),
                ("plain", rule(&["d:4"], false, true)),
            ],
            vec![],
        );
        let got: Vec<String> = labeled_inject(&n)
            .unwrap()
            .into_iter()
            .map(|lt| lt.label)
            .collect();
        assert_eq!(got.len(), 2, "got: {got:?}");
        assert!(
            got.iter()
                .all(|l| l.contains("inj") && l.contains("inject"))
        );
        assert!(
            got[0].contains("a:1") && got[1].contains("b:2"),
            "got: {got:?}"
        );
    }

    #[test]
    fn labeled_middleware_skips_disabled() {
        let n = net(
            vec![],
            vec![
                ("mw-on", mw(&["a:1"], true)),
                ("mw-off", mw(&["b:2"], false)),
            ],
        );
        let got: Vec<String> = labeled_middleware(&n)
            .unwrap()
            .into_iter()
            .map(|lt| lt.label)
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("mw-on"), "got: {got:?}");
    }
}

#[cfg(test)]
mod public_only_tests {
    use super::tests::{TestNetworkConfig, build_network};
    use super::*;

    /// Local, private and metadata destinations, by name and by literal.
    const LOCAL: [&str; 9] = [
        "localhost",
        "LOCALHOST.",
        "foo.localhost",
        "127.0.0.1",
        "::1",
        "[::1]",
        "10.0.0.5",
        "192.168.1.1",
        "169.254.169.254",
    ];

    /// The network of the install boot: `allow-always`, everything
    /// allowed, public destinations only.
    fn install_network(public_only: bool) -> Network {
        let (_, _, network) = build_network(TestNetworkConfig::default());
        network.control().set_policy(Policy::AllowAlways);
        if public_only {
            network.public_only()
        } else {
            network
        }
    }

    #[test]
    fn public_only_denies_local_destinations_by_name_and_literal() {
        let network = install_network(true);
        for host in LOCAL {
            assert!(!network.resolve_target(host, 80).allowed, "{host}");
        }
        let public = network.resolve_target("example.com", 443);
        assert!(public.allowed && public.public_only);
    }

    #[test]
    fn a_normal_network_allows_local_destinations() {
        let network = install_network(false);
        for host in LOCAL {
            let target = network.resolve_target(host, 80);
            assert!(target.allowed && !target.public_only, "{host}");
        }
    }

    /// The resolved addresses decide: `127.1` is no IP literal, but it
    /// resolves to the loopback.
    #[tokio::test]
    async fn public_only_never_dials_a_local_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = |host: &str, public_only: bool| ResolvedTarget {
            public_only,
            ..denied(host, port)
        };
        for host in ["127.0.0.1", "127.1", "localhost"] {
            let e = tcp::dial(&target(host, true)).await.unwrap_err();
            assert!(e.to_string().contains("blocked"), "{host}: {e}");
        }
        assert!(tcp::dial(&target("127.0.0.1", false)).await.is_ok());
    }
}
