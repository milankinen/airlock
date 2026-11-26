//! Startup checks for network target conflicts.
//!
//! Finds config errors before the sandbox starts:
//!  * passthrough targets that overlap intercepted targets
//!  * reverse port forwards that use the same host port

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
                conflicts.push(crate::messages::network::conflicts_with(&a.label, &b.label));
            }
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            crate::messages::network::reverse_forward_host_port_conflict(conflicts.join("\n  "))
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
                conflicts.push(crate::messages::network::conflicts_with(
                    &pt.label, &mw.label,
                ));
            }
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(crate::messages::network::passthrough_intercept_conflict(
            conflicts.join("\n  ")
        ));
    }
}

/// Return true if at least one concrete `(host, port)` matches both
/// targets.
fn targets_overlap(a: &NetworkTarget, b: &NetworkTarget) -> bool {
    ports_overlap(a.port, b.port) && a.host.overlaps(&b.host)
}

fn ports_overlap(a: Option<u16>, b: Option<u16>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(x), Some(y)) => x == y,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the overlap check of target patterns.

    use super::*;

    /// Parse the target pattern `s`, for example `*.example.com:443`.
    fn t(s: &str) -> NetworkTarget {
        let (host, port) = super::super::rules::parse_pattern(s).unwrap();
        NetworkTarget { host, port }
    }

    /// Test that two target patterns overlap only when some host and port
    /// match both. A false answer hides a passthrough conflict, and a false
    /// overlap refuses a valid config.
    ///   1. Take pairs of patterns: exact, other case or trailing dot,
    ///      localhost aliases, wildcards, ports, IP ranges
    ///   2. Check the overlap result in both argument orders
    #[test]
    fn targets_overlap_when_some_host_and_port_match_both() {
        for (a, b, expected) in [
            ("a.example.com", "a.example.com", true),
            ("a.example.com", "b.example.com", false),
            ("Example.com", "example.com", true),
            ("example.com.", "example.com", true),
            ("*.Example.com.", "*.example.com", true),
            ("*.example.com.", "*.prod.Example.com", true),
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
            ("10.0.0.0/8", "10.1.2.3", true),
            ("10.0.0.0/8", "11.1.2.3", false),
            ("10.0.0.0/8", "10.1.0.0/16", true),
            ("10.0.0.0/8", "192.168.0.0/16", false),
            ("10.0.0.0/8", "*.example.com", false),
        ] {
            assert_eq!(targets_overlap(&t(a), &t(b)), expected, "{a} / {b}");
            assert_eq!(targets_overlap(&t(b), &t(a)), expected, "{b} / {a}");
        }
    }
}
