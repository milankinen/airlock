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
}
