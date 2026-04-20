/// Check if a hostname matches a pattern.
///
/// Supported pattern forms:
/// - `*` — matches any host.
/// - `*.<suffix>` — matches exactly one leading label: `<label>.<suffix>`
///   where `<label>` is non-empty and contains no dots. So
///   `*.example.com` matches `api.example.com` but NOT the apex
///   `example.com` and NOT `a.b.example.com`. This follows RFC 6125
///   TLS wildcard rules.
/// - anything else — exact string match, with localhost aliases
///   (`localhost`, `127.0.0.1`, `::1`) treated as equivalent.
///
/// Patterns beginning with `*` but not `*.` (e.g. `*foo.com`) are not
/// wildcards in this scheme and will never match any real hostname.
pub fn host_matches(host: &str, pattern: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix("*.") {
        match host.strip_suffix(suffix) {
            Some(prefix) => match prefix.strip_suffix('.') {
                Some(label) => !label.is_empty() && !label.contains('.'),
                None => false,
            },
            None => false,
        }
    } else if is_localhost(pattern) {
        is_localhost(host)
    } else {
        host == pattern
    }
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
    fn wildcard_matches_exactly_one_leading_label() {
        assert!(host_matches("api.example.com", "*.example.com"));
        assert!(host_matches("www.example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_apex() {
        assert!(!host_matches("example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_multiple_labels() {
        assert!(!host_matches("a.b.example.com", "*.example.com"));
        assert!(!host_matches("x.y.z.example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_requires_leading_label() {
        // Empty label: ".example.com" has no leading label.
        assert!(!host_matches(".example.com", "*.example.com"));
    }

    #[test]
    fn wildcard_is_case_sensitive_and_suffix_exact() {
        assert!(!host_matches("api.example.org", "*.example.com"));
        assert!(!host_matches("api.xample.com", "*.example.com"));
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
