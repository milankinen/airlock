/// Check if a hostname matches a pattern.
/// Supports exact match, `*.domain.com` wildcard, and `*` (match all).
pub fn host_matches(host: &str, pattern: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        // e.g. "*.example.com" → suffix is ".example.com"
        host.ends_with(suffix) || host == &suffix[1..]
    } else {
        host == pattern
    }
}
