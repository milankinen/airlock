use std::rc::Rc;

use super::http::middleware::CompiledMiddleware;
use super::interceptor::Interceptor;
use super::matchers;
use crate::project::MaskedSecret;

/// A resolved network target — parsed from a rule's `allow` or `deny` list
/// at startup. Each target represents one `host[:port]` pattern.
#[derive(Clone, Debug)]
pub struct NetworkTarget {
    pub host: String,
    pub port: Option<u16>,
}

impl NetworkTarget {
    /// Does this target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// A host and port the proxy talks to. The host is always canonical
/// ([`matchers::canonical_host`]: lowercase, no trailing dot), so the
/// services compare endpoints exactly: the guest's DNS keeps the case of
/// a name, and `AUTH.OPENAI.COM` or `api.anthropic.com.` is the same host
/// as the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: matchers::canonical_host(host),
            port,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `host`, or `host:port` for a port other than 443: the `Host` and
    /// `:authority` of a request to the endpoint.
    pub fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn target(&self) -> NetworkTarget {
        NetworkTarget {
            host: self.host.clone(),
            port: Some(self.port),
        }
    }
}

/// The targets of `endpoints`, without duplicates.
pub fn targets_of(endpoints: &[&Endpoint]) -> Vec<NetworkTarget> {
    let mut out: Vec<NetworkTarget> = Vec::new();
    for e in endpoints {
        if !out
            .iter()
            .any(|t| t.host == e.host && t.port == Some(e.port))
        {
            out.push(e.target());
        }
    }
    out
}

/// A compiled middleware script with target patterns for matching.
#[derive(Clone)]
pub struct MiddlewareTarget {
    pub host: String,
    pub port: Option<u16>,
    pub middleware: CompiledMiddleware,
}

impl MiddlewareTarget {
    /// Does this middleware target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// A masked secret as the proxy holds it: shared behind an `Rc` so the
/// per-connection and per-request copies are pointer bumps, not string
/// clones. Derefs to the underlying [`MaskedSecret`].
#[derive(Clone, Debug)]
pub struct InjectedSecret(Rc<MaskedSecret>);

impl InjectedSecret {
    pub fn new(secret: MaskedSecret) -> Self {
        Self(Rc::new(secret))
    }

    /// Whether two handles share the same underlying secret allocation.
    #[cfg(test)]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }
}

impl std::ops::Deref for InjectedSecret {
    type Target = MaskedSecret;

    fn deref(&self) -> &MaskedSecret {
        &self.0
    }
}

/// Masked secrets a rule injects into HTTP headers, paired with one of the
/// rule's allow patterns. One entry per `(rule, allow pattern)`; all entries
/// of a rule share the same underlying secrets.
#[derive(Clone, Debug)]
pub struct InjectTarget {
    pub host: String,
    pub port: Option<u16>,
    pub secrets: Vec<InjectedSecret>,
}

impl InjectTarget {
    /// Does this inject target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

#[derive(Clone)]
pub struct ResolvedTarget {
    pub host: String,
    pub port: u16,
    /// Middleware scripts from all matching middleware rules.
    pub middleware: Vec<CompiledMiddleware>,
    /// Masked secrets to swap in outbound request headers / out of
    /// response headers, from all matching rules with `inject`. Empty for
    /// denied and passthrough connections.
    pub secrets: Vec<InjectedSecret>,
    /// The interceptor that owns the target; it handles every request on
    /// the connection around the upstream send. `None` for denied
    /// connections.
    pub interceptor: Option<Rc<dyn Interceptor>>,
    /// Whether this connection is permitted.
    /// False if denied by policy, deny rule, or no allow rule matched.
    pub allowed: bool,
    /// Skip TLS/HTTP interception and relay the connection as plain TCP.
    /// Set for targets matching a passthrough rule and for localhost
    /// port-forwarded destinations (which may carry non-HTTP protocols
    /// whose first bytes can't be sniffed without deadlocking).
    pub passthrough: bool,
}

impl ResolvedTarget {
    /// True when the allowed connection should skip all interception and
    /// be relayed as plain TCP. Denied connections never passthrough —
    /// they still need to reach the 403 code path.
    pub fn is_passthrough(&self) -> bool {
        self.allowed && self.passthrough
    }
}
