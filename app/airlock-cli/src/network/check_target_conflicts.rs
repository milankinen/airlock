//! Startup checks for network target conflicts.
//!
//! Finds config errors before the sandbox starts:
//!  * passthrough targets that overlap intercepted targets
//!  * reverse port forwards that use the same host port

use super::matchers;
use super::target::NetworkTarget;

/// Reverse port forward with a label for error messages.
pub struct LabeledReverseForward {
    /// Text that identifies the config entry in error messages.
    pub label: String,
    /// Host port that the forward listens on.
    pub host_port: u16,
}

/// Make sure that no two `.guest` reverse port forwards use the same host
/// port. Only one listener can bind `127.0.0.1:<port>`. Two forwards can
/// use the same guest port: two host ports can forward to the same guest
/// service.
/// Returns:
///   Error that names each conflicting pair by label, if conflicts exist.
pub fn check_reverse_forward_conflicts(forwards: &[LabeledReverseForward]) -> anyhow::Result<()> {
    let mut conflicts: Vec<String> = Vec::new();
    for (i, a) in forwards.iter().enumerate() {
        for b in &forwards[i + 1..] {
            if a.host_port == b.host_port {
                conflicts.push(format!("{} conflicts with {}", a.label, b.label));
            }
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "network config: reverse port forward(s) share a host port:\n  {}",
            conflicts.join("\n  ")
        );
    }
}

/// Parsed target with a human-readable label for error messages.
pub struct LabeledTarget {
    /// Text that identifies the config entry in error messages.
    pub label: String,
    /// Parsed target pattern.
    pub target: NetworkTarget,
}

/// Make sure that no passthrough target overlaps an intercepted target.
/// Passthrough means "no interception". Middleware and inject need
/// interception. If both apply to the same destination, one of them wins
/// silently.
/// Args:
///  - `passthrough`: Passthrough targets
///  - `middleware`: Intercepted targets (middleware, inject and services).
///
/// Returns:
///   Error that names each conflicting pair, if conflicts exist. The user
///   can then fix the config.
pub fn check_passthrough_conflicts(
    passthrough: &[LabeledTarget],
    middleware: &[LabeledTarget],
) -> anyhow::Result<()> {
    let mut conflicts: Vec<String> = Vec::new();
    for pt in passthrough {
        for mw in middleware {
            if targets_overlap(&pt.target, &mw.target) {
                conflicts.push(format!("{} conflicts with {}", pt.label, mw.label));
            }
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "network config: passthrough target(s) overlap intercepting (middleware/inject) target(s):\n  {}",
            conflicts.join("\n  ")
        );
    }
}

/// Return true if at least one concrete `(host, port)` matches both
/// targets. The wildcard rules are the same as in
/// [`super::matchers::host_matches`]:
///
/// - `*` matches all hosts.
/// - `*.<suffix>` matches all subdomains of `<suffix>` at all depths.
///   `*.example.com` matches `api.example.com` and `a.b.example.com`.
/// - All other patterns are exact literals (with localhost aliases).
///
/// The pattern syntax is narrow. Thus a small case analysis is sufficient,
/// and no generic regex intersection is necessary.
fn targets_overlap(a: &NetworkTarget, b: &NetworkTarget) -> bool {
    ports_overlap(a.port, b.port) && hosts_overlap(&a.host, &b.host)
}

fn ports_overlap(a: Option<u16>, b: Option<u16>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(x), Some(y)) => x == y,
    }
}

fn hosts_overlap(a: &str, b: &str) -> bool {
    if a == "*" || b == "*" {
        return true;
    }
    match (a.strip_prefix("*."), b.strip_prefix("*.")) {
        // Two wildcards: *.A and *.B overlap only if A == B, or if one suffix
        // is a dot-separated sub-suffix of the other (for example
        // "example.com" and "prod.example.com"). A host such as
        // `x.prod.example.com` matches both `*.example.com` and
        // `*.prod.example.com`.
        (Some(sa), Some(sb)) => {
            sa == sb
                || sa.strip_suffix(sb).is_some_and(|p| p.ends_with('.'))
                || sb.strip_suffix(sa).is_some_and(|p| p.ends_with('.'))
        }
        // Wildcard and literal: use host_matches, so the rules stay the same.
        (Some(_sa), None) => matchers::host_matches(b, a),
        (None, Some(_sb)) => matchers::host_matches(a, b),
        (None, None) => a == b || (is_localhost(a) && is_localhost(b)),
    }
}

fn is_localhost(s: &str) -> bool {
    s == "localhost" || s == "127.0.0.1" || s == "::1"
}

#[cfg(test)]
mod tests {
    //! Tests for the overlap check of target patterns.

    use super::*;

    /// Parse the target pattern `s`, for example `*.example.com:443`.
    fn t(s: &str) -> NetworkTarget {
        let (host, port) = super::super::rules::parse_pattern(s).unwrap();
        NetworkTarget {
            host: host.to_string(),
            port,
        }
    }

    /// Test that two target patterns overlap only when some host and port
    /// match both. A false answer hides a passthrough conflict, and a false
    /// overlap refuses a valid config.
    ///   1. Take pairs of patterns: exact, localhost aliases, wildcards, ports
    ///   2. Check the overlap result in both argument orders
    #[test]
    fn targets_overlap_when_some_host_and_port_match_both() {
        for (a, b, expected) in [
            ("a.example.com", "a.example.com", true),
            ("a.example.com", "b.example.com", false),
            ("localhost", "127.0.0.1", true),
            ("127.0.0.1", "::1", true),
            ("::1", "localhost", true),
            ("*", "anything.example.com", true),
            ("*", "*.foo", true),
            ("*.example.com", "api.example.com", true),
            ("*.example.com", "a.b.example.com", true),
            ("*.example.com", "example.com", false),
            ("*.example.com", "example.org", false),
            ("*.example.com", "xample.com", false),
            ("*.example.com", "*.prod.example.com", true),
            ("*.example.com", "*.example.com", true),
            ("*.example.com", "*.foo.com", false),
            ("*.example.com", "*.myexample.com", false),
            ("example.com", "example.com:443", true),
            ("example.com:80", "example.com:443", false),
            ("*.example.com:80", "api.example.com:443", false),
        ] {
            assert_eq!(targets_overlap(&t(a), &t(b)), expected, "{a} / {b}");
            assert_eq!(targets_overlap(&t(b), &t(a)), expected, "{b} / {a}");
        }
    }
}
