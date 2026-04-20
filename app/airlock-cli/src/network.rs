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
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use tokio::sync::broadcast;

pub use self::control::NetworkControl;
pub use self::deny_reporter::DenyReporter;
use crate::config::config::Policy;
use crate::network::http::middleware::CompiledMiddleware;
use crate::network::target::{MiddlewareTarget, NetworkTarget, ResolvedTarget};
use crate::project::Project;

const NETWORK_EVENTS_BUFFER: usize = 10;

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
    let rule_targets = rules::resolve(net);
    let middleware_targets = rules::resolve_middleware(net, &project.vault, &log)?;

    check_target_conflicts::check_passthrough_conflicts(
        &labeled_passthrough(net),
        &labeled_middleware(net),
    )?;

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
        "network: {} allow, {} deny, {} passthrough, {} middleware targets",
        rule_targets.allow.len(),
        rule_targets.deny.len(),
        rule_targets.passthrough.len(),
        middleware_targets.len()
    );

    Ok(Network {
        state: Arc::new(RwLock::new(NetworkState { policy: net.policy })),
        tls_client: Arc::new(tls_client),
        interceptor: Rc::new(interceptor),
        allow_targets: rule_targets.allow,
        deny_targets: rule_targets.deny,
        passthrough_targets: rule_targets.passthrough,
        middleware_targets,
        port_forwards,
        socket_map,
        events,
        next_id: AtomicU64::new(0),
        deny_reporter: DenyReporter::new(),
    })
}

/// Extract labeled passthrough targets from the config for conflict
/// checking. Each entry carries a display label — the validator treats
/// that label as opaque, so formatting lives here where the config shape
/// is known.
fn labeled_passthrough(
    net: &crate::config::config::Network,
) -> Vec<check_target_conflicts::LabeledTarget> {
    let mut out = Vec::new();
    for (rule_name, rule) in &net.rules {
        if !rule.enabled || !rule.passthrough {
            continue;
        }
        for allow in &rule.allow {
            let (host, port) = rules::parse_target(allow);
            out.push(check_target_conflicts::LabeledTarget {
                label: format!("rule `{rule_name}` allow=`{allow}` (passthrough)"),
                target: NetworkTarget {
                    host: host.to_string(),
                    port: port.and_then(|p| p.parse::<u16>().ok()),
                },
            });
        }
    }
    out
}

/// Extract labeled middleware targets from the config for conflict
/// checking. Paired with [`labeled_passthrough`].
fn labeled_middleware(
    net: &crate::config::config::Network,
) -> Vec<check_target_conflicts::LabeledTarget> {
    let mut out = Vec::new();
    for (mw_name, mw) in &net.middleware {
        if !mw.enabled {
            continue;
        }
        for target_str in &mw.target {
            let (host, port) = rules::parse_target(target_str);
            out.push(check_target_conflicts::LabeledTarget {
                label: format!("middleware `{mw_name}` target=`{target_str}`"),
                target: NetworkTarget {
                    host: host.to_string(),
                    port: port.and_then(|p| p.parse::<u16>().ok()),
                },
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
    /// Port forward mappings: guest_port → host_port.
    port_forwards: HashMap<u16, u16>,
    /// Guest socket path → host socket path mapping for Unix socket forwarding.
    pub(super) socket_map: HashMap<String, PathBuf>,
    /// Network events to subscribe to
    // TODO: this NetworkEvent should be Network event agnostic to any TUI
    events: broadcast::Sender<airlock_monitor::NetworkEvent>,
    /// Monotonic counter for connection ids. Used by the TUI to pair
    /// `Disconnect` events with their originating `Connect`.
    next_id: AtomicU64,
    /// Host → guest notifier for every denied connection. Populated once
    /// the supervisor handshake completes; no-op until then.
    pub(super) deny_reporter: Rc<DenyReporter>,
}

impl Network {
    pub fn events(&self) -> broadcast::Receiver<airlock_monitor::NetworkEvent> {
        self.events.subscribe()
    }

    /// Handle used by the host to wire up the supervisor client after the
    /// vsock handshake. See [`DenyReporter::attach`].
    pub fn deny_reporter(&self) -> Rc<DenyReporter> {
        self.deny_reporter.clone()
    }

    /// Return a thread-safe handle for mutating runtime state. Handed to the
    /// TUI by [`MonitorRuntime::launch`]; the TUI uses it to flip policy /
    /// toggle rules without touching `Network` internals.
    pub fn control(&self) -> NetworkControl {
        NetworkControl::new(self.state.clone())
    }

    /// Current top-level policy. Called on the hot connect path — one
    /// uncontended `RwLock` read.
    pub fn policy(&self) -> Policy {
        self.state.read().policy
    }

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
        let policy = self.policy();

        // deny-always denies everything.
        if matches!(policy, Policy::DenyAlways) {
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

        let allowed = self.is_allowed(host, port, policy) || port_forwarded;
        let middleware = if allowed {
            self.collect_middleware(host, port)
        } else {
            vec![]
        };

        // Port-forwarded destinations always passthrough — the guest side
        // is talking to an arbitrary protocol on localhost, not necessarily
        // HTTP; intercepting would break non-HTTP forwards (e.g. Postgres).
        let passthrough = allowed && (port_forwarded || self.is_passthrough_target(host, port));

        ResolvedTarget {
            host: host.to_string(),
            port,
            middleware,
            allowed,
            passthrough,
        }
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
        for target in &self.deny_targets {
            if target.matches(host, port) {
                return false;
            }
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
        allowed: false,
        passthrough: false,
    }
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

#[cfg(test)]
mod labeled_target_tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::config::{self, MiddlewareRule, NetworkRule};

    fn rule(allow: &[&str], passthrough: bool, enabled: bool) -> NetworkRule {
        NetworkRule {
            enabled,
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: vec![],
            passthrough,
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
    ) -> config::Network {
        config::Network {
            policy: Policy::DenyByDefault,
            rules: rules.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            middleware: middleware
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
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
            .into_iter()
            .map(|lt| lt.label)
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("pt-on"), "got: {got:?}");
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
            .into_iter()
            .map(|lt| lt.label)
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("mw-on"), "got: {got:?}");
    }
}
