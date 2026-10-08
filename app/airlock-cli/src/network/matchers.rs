//! Host name pattern matching.
//!
//! Matches host names against network rule patterns, and gives one canonical
//! form for each host name.

/// Return true if a host name matches a pattern.
///
/// Supported pattern forms:
/// - `*`: matches all hosts.
/// - `*.<suffix>`: matches all subdomains of `<suffix>`, also nested
///   subdomains. Thus `*.example.com` matches `api.example.com` and
///   `a.b.example.com`, but NOT the apex `example.com`.
/// - All other patterns: exact string match. The localhost aliases
///   (`localhost`, `127.0.0.1`, `::1`) are equal.
///
/// A pattern that starts with `*` but not with `*.` (for example
/// `*foo.com`) is not a wildcard. It never matches a real host name.
///
/// The comparison ignores case and one trailing dot.
///
/// This is different from RFC 6125 (TLS certificate wildcard rules) on
/// purpose. RFC 6125 limits `*` to one DNS label. This function follows the
/// convention of HTTP proxies and CDNs (Nginx, Envoy, Cloudflare), where
/// `*.example.com` matches all subdomain depths. If strict single-label
/// matching becomes necessary, this function needs a new design.
pub fn host_matches(host: &str, pattern: &str) -> bool {
    // DNS and `TcpStream::connect` ignore case and treat `host.` as `host`.
    // Without canonical forms, `SECRET.example.com` or `secret.example.com.`
    // would pass a `deny secret.example.com` rule and still resolve to the
    // blocked host. That is a policy bypass.
    let host = canonical_host(host);
    let pattern = canonical_host(pattern);
    let (host, pattern) = (host.as_str(), pattern.as_str());

    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix("*.") {
        match host.strip_suffix(suffix) {
            // The prefix must be at least "x.": a non-empty label and a dot.
            Some(prefix) => prefix.len() > 1 && prefix.ends_with('.'),
            None => false,
        }
    } else if is_localhost(pattern) {
        is_localhost(host)
    } else {
        host == pattern
    }
}

/// Get the canonical form of a host name or host pattern. Use it to compare
/// host names with no effect from case or a trailing dot.
///
/// The function makes the name lowercase and removes one trailing `.` (the
/// DNS root label). The `*` and `*.` wildcard markers do not change. The
/// network services also make their endpoints with this function
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
    //! Tests for host pattern matching.

    use super::*;

    /// Test that host matching follows the wildcard and localhost alias
    /// rules. Rules and middleware select their hosts with this match.
    ///   1. Take hosts and patterns: `*`, `*.suffix`, exact, aliases
    ///   2. Check the match result of each pair
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
            // `*` without a dot is not a wildcard.
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

    /// Test that case and a trailing dot in a host do not evade a pattern.
    /// Both forms reach the same server, so a deny must still apply.
    ///   1. Take hosts and patterns in mixed case and with trailing dots
    ///   2. Check the match result of each pair
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
