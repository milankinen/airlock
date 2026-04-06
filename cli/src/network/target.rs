use super::http::middleware::CompiledMiddleware;
use super::matchers;

/// A resolved network target — parsed from config rules at startup.
/// Each target represents one allowed `[http:]host[:port]` pattern
/// with optional compiled HTTP middleware scripts.
#[derive(Clone)]
pub struct NetworkTarget {
    pub host: String,
    pub port: Option<u16>,
    /// If true, only HTTP traffic is allowed; non-HTTP is rejected.
    pub http_only: bool,
    pub middleware: Vec<CompiledMiddleware>,
}

impl NetworkTarget {
    /// Does this target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }

    /// Should TLS be passed through (no MITM) for this target?
    /// Targets without middleware get passthrough, unless http_only
    /// is set (needs interception to verify HTTP).
    pub fn is_passthrough(&self) -> bool {
        self.middleware.is_empty() && !self.http_only
    }
}

/// Find the first target matching host:port, or None if denied.
pub fn find_match<'a>(
    targets: &'a [NetworkTarget],
    host: &str,
    port: u16,
) -> Option<&'a NetworkTarget> {
    targets.iter().find(|t| t.matches(host, port))
}
