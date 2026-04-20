use super::http::middleware::CompiledMiddleware;
use super::matchers;

/// A resolved network target — parsed from a rule's `allow` or `deny` list
/// at startup. Each target represents one `host[:port]` pattern.
#[derive(Clone)]
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

#[derive(Clone)]
pub struct ResolvedTarget {
    pub host: String,
    pub port: u16,
    /// Middleware scripts from all matching middleware rules.
    pub middleware: Vec<CompiledMiddleware>,
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
