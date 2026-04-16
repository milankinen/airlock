/// Check if a hostname matches a pattern.
/// Supports exact match, `*.domain.com` wildcard, and `*` (match all).
/// Localhost aliases (`localhost`, `127.0.0.1`, `::1`) are treated as
/// equivalent — a pattern of `localhost` matches host `127.0.0.1` and
/// vice versa.
pub fn host_matches(host: &str, pattern: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        // e.g. "*.example.com" → suffix is ".example.com"
        host.ends_with(suffix) || host == &suffix[1..]
    } else if is_localhost(pattern) {
        is_localhost(host)
    } else {
        host == pattern
    }
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}
