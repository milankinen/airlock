/// Check if a hostname matches a pattern.
///
/// Supported pattern forms:
/// - `*` — matches any host.
/// - `*.<suffix>` — matches any subdomain of `<suffix>`, including
///   nested subdomains. So `*.example.com` matches `api.example.com`
///   and `a.b.example.com`, but NOT the apex `example.com`.
/// - anything else — exact string match, with localhost aliases
///   (`localhost`, `127.0.0.1`, `::1`) treated as equivalent.
///
/// Patterns beginning with `*` but not `*.` (e.g. `*foo.com`) are not
/// wildcards in this scheme and will never match any real hostname.
///
/// Both the host and the pattern are canonicalized before comparison
/// (lowercased, with a single trailing dot stripped), because DNS and
/// `TcpStream::connect` are case-insensitive and treat `host.` as `host`.
/// Without this, `SECRET.example.com` or `secret.example.com.` would slip
/// past a `deny secret.example.com` rule while still resolving to the
/// blocked host — a policy bypass.
///
/// This intentionally deviates from RFC 6125 (TLS certificate wildcard
/// rules), which restricts `*` to a single DNS label. We follow the
/// convention used by modern HTTP proxies and CDNs (Nginx, Envoy,
/// Cloudflare) where `*.example.com` matches all subdomain depths.
/// If strict single-label matching is needed in the future, this
/// function must be redesigned.
pub fn host_matches(host: &str, pattern: &str) -> bool {
    let host = canonical_host(host);
    let pattern = canonical_host(pattern);
    let (host, pattern) = (host.as_str(), pattern.as_str());

    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix("*.") {
        match host.strip_suffix(suffix) {
            // prefix must be at least "x." — a non-empty label followed by a dot.
            Some(prefix) => prefix.len() > 1 && prefix.ends_with('.'),
            None => false,
        }
    } else if is_localhost(pattern) {
        is_localhost(host)
    } else {
        host == pattern
    }
}

/// Canonicalize a hostname (or host pattern) for case- and trailing-dot-
/// insensitive comparison: lowercase it and strip a single trailing `.`
/// (the DNS root label). The `*` and `*.` wildcard markers are ASCII and
/// pass through unchanged.
fn canonical_host(host: &str) -> String {
    let host = host.strip_suffix('.').unwrap_or(host);
    host.to_ascii_lowercase()
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_anything() {
        assert!(host_matches("anything.example.com", "*"));
        assert!(host_matches("example.com", "*"));
        assert!(host_matches("localhost", "*"));
    }

    #[test]
    fn wildcard_matches_single_subdomain() {
        assert!(host_matches("api.example.com", "*.example.com"));
        assert!(host_matches("www.example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_matches_nested_subdomains() {
        assert!(host_matches("a.b.example.com", "*.example.com"));
        assert!(host_matches("x.y.z.example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_without_subdomain() {
        assert!(!host_matches("example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_requires_leading_label() {
        // Empty label: ".example.com" has no leading label.
        assert!(!host_matches(".example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_suffix_is_exact() {
        assert!(!host_matches("api.example.org", "*.example.com"));
        assert!(!host_matches("api.xample.com", "*.example.com"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        // Regression: DNS is case-insensitive, so an uppercased host must
        // not evade a lowercase deny rule (nor a lowercase host a rule with
        // uppercase in it).
        assert!(host_matches("SECRET.example.com", "secret.example.com"));
        assert!(host_matches("secret.example.com", "SECRET.EXAMPLE.COM"));
        assert!(host_matches("API.EXAMPLE.COM", "*.example.com"));
        assert!(host_matches("api.example.com", "*.EXAMPLE.COM"));
        assert!(host_matches("LOCALHOST", "localhost"));
    }

    #[test]
    fn trailing_dot_is_ignored() {
        // Regression: `host.` resolves to `host`, so a trailing dot must
        // not evade a rule written without one.
        assert!(host_matches("secret.example.com.", "secret.example.com"));
        assert!(host_matches("api.example.com.", "*.example.com"));
        // The apex with a trailing dot still must not match a `*.` wildcard.
        assert!(!host_matches("example.com.", "*.example.com"));
    }

    #[test]
    fn exact_literal_match() {
        assert!(host_matches("example.com", "example.com"));
        assert!(!host_matches("api.example.com", "example.com"));
    }

    #[test]
    fn localhost_aliases_are_equivalent() {
        assert!(host_matches("127.0.0.1", "localhost"));
        assert!(host_matches("localhost", "127.0.0.1"));
        assert!(host_matches("::1", "localhost"));
        assert!(host_matches("localhost", "::1"));
    }

    #[test]
    fn non_wildcard_star_pattern_matches_nothing() {
        // "*foo.com" is not a supported wildcard form: it starts with `*`
        // but not `*.`, so it's treated as a literal — and hostnames
        // never contain `*`, so nothing matches.
        assert!(!host_matches("foo.com", "*foo.com"));
        assert!(!host_matches("api.foo.com", "*foo.com"));
    }
}
