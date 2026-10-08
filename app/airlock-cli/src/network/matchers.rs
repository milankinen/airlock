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
/// pass through unchanged. The one canonical form of a host name: the
/// network services build their endpoints with it too
/// ([`crate::network::target::Endpoint`]).
pub fn canonical_host(host: &str) -> String {
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
    fn host_matching_follows_wildcard_and_alias_rules() {
        for (host, pattern, expected) in [
            ("anything.example.com", "*", true),
            ("localhost", "*", true),
            ("api.example.com", "*.example.com", true),
            ("x.y.z.example.com", "*.example.com", true),
            ("example.com", "*.example.com", false),
            (".example.com", "*.example.com", false),
            ("api.example.org", "*.example.com", false),
            ("api.xample.com", "*.example.com", false),
            ("example.com", "example.com", true),
            ("api.example.com", "example.com", false),
            ("foo.com", "*foo.com", false),
            ("api.foo.com", "*foo.com", false),
            ("127.0.0.1", "localhost", true),
            ("localhost", "127.0.0.1", true),
            ("::1", "localhost", true),
            ("localhost", "::1", true),
        ] {
            assert_eq!(host_matches(host, pattern), expected, "{host} ~ {pattern}");
        }
    }

    #[test]
    fn host_case_and_trailing_dot_do_not_evade_pattern() {
        for (host, pattern, expected) in [
            ("SECRET.example.com", "secret.example.com", true),
            ("secret.example.com", "SECRET.EXAMPLE.COM", true),
            ("API.EXAMPLE.COM", "*.example.com", true),
            ("api.example.com", "*.EXAMPLE.COM", true),
            ("LOCALHOST", "localhost", true),
            ("secret.example.com.", "secret.example.com", true),
            ("api.example.com.", "*.example.com", true),
            ("example.com.", "*.example.com", false),
        ] {
            assert_eq!(host_matches(host, pattern), expected, "{host} ~ {pattern}");
        }
    }
}
