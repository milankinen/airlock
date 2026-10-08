//! Startup-time validation of network targets.
//!
//! Two checks live here:
//!
//! 1. Overlap between passthrough rule targets and middleware targets —
//!    the two are semantically incompatible (passthrough skips
//!    interception; middleware requires it), so any overlap is a
//!    config error. Both sides use the same `host[:port]` pattern
//!    syntax, which is narrow enough that intersection can be decided
//!    by a small case analysis (see [`targets_overlap`]) rather than
//!    a generic regex-intersection engine.
//!
//! 2. Duplicate host-side ports across `.guest` reverse port forwards
//!    ([`check_reverse_forward_conflicts`]). Two `.guest` entries
//!    can't both bind the same `127.0.0.1:<port>`.

use super::matchers;
use super::target::NetworkTarget;

/// A labeled reverse port forward, used for error messages when two
/// `.guest` entries collide on the same host port.
pub struct LabeledReverseForward {
    pub label: String,
    pub host_port: u16,
}

/// Reject configs where two `.guest` reverse port forwards share a
/// host port — only one listener can bind `127.0.0.1:<port>`. Same
/// guest port on the other side is fine (two host ports forwarding
/// into the same guest service is legal).
///
/// Error messages name every offending pair by label.
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

/// A parsed target tagged with a human-readable label, used for error
/// messages when a conflict is reported.
pub struct LabeledTarget {
    pub label: String,
    pub target: NetworkTarget,
}

/// Reject configs where any passthrough target overlaps any middleware
/// target. Passthrough means "no interception," middleware needs
/// interception — they can't both apply to the same destination without
/// one silently winning.
///
/// Error messages name every offending (passthrough, middleware) pair so
/// the user can fix the config directly.
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

/// True iff there exists at least one concrete `(host, port)` that both
/// targets would match, using the same wildcard semantics as
/// [`super::matchers::host_matches`]:
///
/// - `*` matches any host.
/// - `*.<suffix>` matches any subdomain of `<suffix>` at any depth —
///   `*.example.com` matches both `api.example.com` and `a.b.example.com`.
/// - anything else is an exact literal (with localhost aliases).
///
/// Two `*.suffix` wildcards overlap when their suffixes are equal, or when
/// one suffix is a dot-separated sub-suffix of the other (e.g.
/// `*.example.com` and `*.prod.example.com` overlap because any host
/// matching the narrower pattern also matches the broader one). Wildcard ×
/// literal reduces to "does the literal match the wildcard." `*` vs
/// anything always overlaps.
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
        // Both wildcards: *.A and *.B overlap iff A==B, or one suffix contains
        // the other as a dot-separated sub-suffix (e.g. "example.com" and
        // "prod.example.com"), because a multi-label host like `x.prod.example.com`
        // would match both `*.example.com` and `*.prod.example.com` at runtime.
        (Some(sa), Some(sb)) => {
            sa == sb
                || sa.strip_suffix(sb).is_some_and(|p| p.ends_with('.'))
                || sb.strip_suffix(sa).is_some_and(|p| p.ends_with('.'))
        }
        // Wildcard × literal: delegate to host_matches so semantics stay in sync.
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
    use super::*;

    fn t(s: &str) -> NetworkTarget {
        let (host, port) = super::super::rules::parse_pattern(s).unwrap();
        NetworkTarget {
            host: host.to_string(),
            port,
        }
    }

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
