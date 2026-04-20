//! Startup-time validation of network targets.
//!
//! The only check today is detecting overlap between passthrough rule
//! targets and middleware targets. Both sides use the same
//! `host[:port]` pattern syntax, which is narrow enough that intersection
//! can be decided by a small case analysis (see [`targets_overlap`])
//! rather than a generic regex-intersection engine.

use super::target::NetworkTarget;

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
            "network config: passthrough target(s) overlap middleware target(s):\n  {}",
            conflicts.join("\n  ")
        );
    }
}

/// True iff there exists at least one concrete `(host, port)` that both
/// targets would match, using the same pattern semantics as
/// [`super::matchers::host_matches`]:
///
/// - `*` matches any host.
/// - `*.<suffix>` matches exactly one leading label (no apex, no
///   multi-label prefix).
/// - anything else is an exact literal (with localhost aliases).
///
/// Under these rules, two `*.suffix` wildcards overlap iff their
/// suffixes are identical — a multi-label difference can never be
/// bridged by a single-label wildcard. Wildcard × literal reduces to
/// "does the literal match the wildcard." `*` vs anything always
/// overlaps.
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
        (Some(sa), Some(sb)) => sa == sb,
        (Some(suffix), None) => wildcard_matches_literal(suffix, b),
        (None, Some(suffix)) => wildcard_matches_literal(suffix, a),
        (None, None) => a == b || (is_localhost(a) && is_localhost(b)),
    }
}

/// `*.<suffix>` matches `host` iff `host = <label>.<suffix>` where
/// `<label>` is non-empty and contains no dots.
fn wildcard_matches_literal(suffix: &str, host: &str) -> bool {
    match host.strip_suffix(suffix) {
        Some(prefix) => match prefix.strip_suffix('.') {
            Some(label) => !label.is_empty() && !label.contains('.'),
            None => false,
        },
        None => false,
    }
}

fn is_localhost(s: &str) -> bool {
    s == "localhost" || s == "127.0.0.1" || s == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NetworkTarget {
        let (host, port) = super::super::rules::parse_target(s);
        NetworkTarget {
            host: host.to_string(),
            port: port.and_then(|p| p.parse::<u16>().ok()),
        }
    }

    fn labeled(label: &str, spec: &str) -> LabeledTarget {
        LabeledTarget {
            label: label.to_string(),
            target: t(spec),
        }
    }

    // ── Overlap unit tests ────────────────────────────────

    #[test]
    fn exact_literals_overlap_only_when_equal() {
        assert!(targets_overlap(&t("a.example.com"), &t("a.example.com")));
        assert!(!targets_overlap(&t("a.example.com"), &t("b.example.com")));
    }

    #[test]
    fn localhost_aliases_overlap() {
        // `::1` has embedded colons, so `parse_target`'s rsplit-on-`:` would
        // mis-parse it — build the IPv6 target directly.
        let ipv6 = NetworkTarget {
            host: "::1".to_string(),
            port: None,
        };
        assert!(targets_overlap(&t("localhost"), &t("127.0.0.1")));
        assert!(targets_overlap(&t("127.0.0.1"), &ipv6));
        assert!(targets_overlap(&ipv6, &t("localhost")));
    }

    #[test]
    fn wildcard_star_matches_everything() {
        assert!(targets_overlap(&t("*"), &t("anything.example.com")));
        assert!(targets_overlap(&t("*"), &t("*.foo")));
        assert!(targets_overlap(&t("*"), &t("*")));
    }

    #[test]
    fn wildcard_subdomain_covers_single_label_subdomain_only() {
        assert!(targets_overlap(&t("*.example.com"), &t("api.example.com")));
        // Apex is NOT matched by `*.example.com` (RFC 6125).
        assert!(!targets_overlap(&t("*.example.com"), &t("example.com")));
        // Multi-label host is NOT matched by a single-label wildcard.
        assert!(!targets_overlap(&t("*.example.com"), &t("a.b.example.com")));
    }

    #[test]
    fn wildcard_subdomain_misses_unrelated_host() {
        assert!(!targets_overlap(&t("*.example.com"), &t("example.org")));
        assert!(!targets_overlap(&t("*.example.com"), &t("xample.com")));
    }

    #[test]
    fn wildcard_wildcard_overlaps_only_when_suffixes_equal() {
        // Different suffixes can never overlap: a single-label wildcard
        // can't bridge a multi-label difference.
        assert!(!targets_overlap(
            &t("*.example.com"),
            &t("*.prod.example.com")
        ));
        assert!(!targets_overlap(&t("*.example.com"), &t("*.foo.com")));
        // Same suffix overlaps.
        assert!(targets_overlap(&t("*.example.com"), &t("*.example.com")));
        // `*` always overlaps.
        assert!(targets_overlap(&t("*"), &t("*.example.com")));
    }

    #[test]
    fn port_any_overlaps_with_specific_port() {
        assert!(targets_overlap(&t("example.com"), &t("example.com:443")));
        assert!(targets_overlap(&t("example.com:443"), &t("example.com")));
    }

    #[test]
    fn port_mismatch_suppresses_overlap() {
        assert!(!targets_overlap(
            &t("example.com:80"),
            &t("example.com:443")
        ));
        assert!(!targets_overlap(
            &t("*.example.com:80"),
            &t("api.example.com:443")
        ));
    }

    // ── check_passthrough_conflicts integration tests ────

    #[test]
    fn no_conflict_when_targets_disjoint() {
        let pt = vec![labeled("rule `pt`", "db.example.com:5432")];
        let mw = vec![labeled("middleware `api`", "api.example.com")];
        assert!(check_passthrough_conflicts(&pt, &mw).is_ok());
    }

    #[test]
    fn conflict_reports_both_labels() {
        let pt = vec![labeled(
            "rule `pt-db` allow=`db.example.com:5432`",
            "db.example.com:5432",
        )];
        let mw = vec![labeled(
            "middleware `mitm-db` target=`db.example.com:5432`",
            "db.example.com:5432",
        )];
        let err = check_passthrough_conflicts(&pt, &mw)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pt-db"), "missing rule name: {err}");
        assert!(err.contains("mitm-db"), "missing mw name: {err}");
    }

    #[test]
    fn wildcard_passthrough_catches_literal_middleware() {
        let pt = vec![labeled("pt-zone", "*.example.com")];
        let mw = vec![labeled("mitm-api", "api.example.com:443")];
        assert!(check_passthrough_conflicts(&pt, &mw).is_err());
    }

    #[test]
    fn wildcard_middleware_caught_by_literal_passthrough() {
        let pt = vec![labeled("pt-api", "api.example.com:443")];
        let mw = vec![labeled("mitm-zone", "*.example.com")];
        assert!(check_passthrough_conflicts(&pt, &mw).is_err());
    }

    #[test]
    fn nested_wildcards_with_different_suffixes_do_not_conflict() {
        // `*.example.com` only matches one-label subdomains, so it
        // cannot overlap a deeper wildcard like `*.prod.example.com`.
        let pt = vec![labeled("pt", "*.example.com")];
        let mw = vec![labeled("mitm", "*.prod.example.com")];
        assert!(check_passthrough_conflicts(&pt, &mw).is_ok());
    }

    #[test]
    fn disjoint_wildcards_pass() {
        let pt = vec![labeled("pt", "*.internal")];
        let mw = vec![labeled("mitm", "*.example.com")];
        assert!(check_passthrough_conflicts(&pt, &mw).is_ok());
    }

    #[test]
    fn port_disjoint_avoids_false_positive() {
        let pt = vec![labeled("pt", "api.example.com:5432")];
        let mw = vec![labeled("mitm", "api.example.com:443")];
        assert!(check_passthrough_conflicts(&pt, &mw).is_ok());
    }
}
