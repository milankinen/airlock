use super::http::middleware::CompiledMiddleware;
use super::matchers;

/// A resolved network target — parsed from a rule's `allow` list at startup.
/// Each target represents one `host[:port]` pattern with optional compiled
/// HTTP middleware scripts.
#[derive(Clone)]
pub struct NetworkTarget {
    pub host: String,
    pub port: Option<u16>,
    pub middleware: Vec<CompiledMiddleware>,
}

impl NetworkTarget {
    /// Does this target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

#[derive(Clone)]
pub struct ResolvedTarget {
    pub host: String,
    pub port: u16,
    /// Middleware scripts from all matching allow rules.
    pub middleware: Vec<CompiledMiddleware>,
    /// Whether this connection is permitted.
    /// False if any deny pattern matched or no allow pattern matched.
    pub allowed: bool,
}

impl ResolvedTarget {
    /// Should TLS be passed through (no MITM) for this target?
    /// Only allowed targets without middleware get raw passthrough.
    pub fn is_passthrough(&self) -> bool {
        self.allowed && self.middleware.is_empty()
    }
}
