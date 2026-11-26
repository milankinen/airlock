//! Typed hosts and host patterns.
//!
//! A host of a connection is a DNS name or an IP address, and the rules
//! compare it in that form, not as text. One IP address has many text
//! forms, for example `169.254.169.254` and `::ffff:169.254.169.254`. A
//! name and an address never have the same text, so each host and each
//! rule pattern parses to one form one time.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;

/// Destination host of a connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    /// DNS name in canonical form ([`canonical_name`]).
    Name(String),
    /// IP address in canonical form: an IPv4-mapped IPv6 address is the
    /// IPv4 address.
    Ip(IpAddr),
}

impl Host {
    /// Parse a host, for example the host of a sandbox connection. An IP
    /// literal (also in brackets) or a numeric IPv4 form such as `127.1`
    /// is an address. All other text is a name, possibly not valid
    /// ([`Host::is_valid`]).
    pub fn parse(host: &str) -> Self {
        // The proxy dials the canonical name, so `127.0.0.1.` is an address.
        let name = canonical_name(host);
        let literal = name
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&name);
        if let Ok(ip) = literal.parse::<IpAddr>() {
            return Host::Ip(ip.to_canonical());
        }
        // The URL host parser reads the numeric IPv4 forms as the resolver
        // does.
        match url::Host::parse(&name) {
            Ok(url::Host::Ipv4(ip)) => Host::Ip(IpAddr::V4(ip)),
            _ => Host::Name(name),
        }
    }

    /// Return true if the host is an IP address or a valid DNS host name.
    /// The resolver can read other text in ways that the rules do not
    /// know, for example `fe80::1%en0`.
    pub fn is_valid(&self) -> bool {
        match self {
            Host::Name(name) => is_host_name(name),
            Host::Ip(_) => true,
        }
    }

    /// Return true if the host is `localhost`, `127.0.0.1` or `::1`.
    pub fn is_localhost(&self) -> bool {
        match self {
            Host::Name(name) => name == "localhost",
            Host::Ip(ip) => is_localhost_ip(*ip),
        }
    }
}

impl fmt::Display for Host {
    /// Write the name or the address. An IPv6 address has no brackets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Name(name) => f.write_str(name),
            Host::Ip(ip) => ip.fmt(f),
        }
    }
}

/// Host part of a network rule target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPattern {
    /// `*`: all hosts.
    Any,
    /// `*.<suffix>`: all subdomains of the suffix at all depths, but not
    /// the suffix itself. The suffix is canonical.
    Suffix(String),
    /// One canonical name.
    Name(String),
    /// IP addresses in a range. A single address is a /32 or /128 range.
    Net(IpNet),
}

impl HostPattern {
    /// Parse the host part of a rule target, for example `*.example.com`,
    /// `api.example.com`, `10.0.0.5` or `10.0.0.0/8`.
    /// Returns:
    ///   The pattern, or an error text if the pattern can never match a
    ///   host: a bad IP range, a `*` that is not the full host or a `*.`
    ///   prefix, or a name that is not an IP address but looks like one.
    pub fn parse(pattern: &str) -> Result<Self, PatternError> {
        if pattern == "*" {
            return Ok(HostPattern::Any);
        }
        if pattern.contains('/') {
            let net: IpNet = pattern.parse().map_err(|_| PatternError::IpRange)?;
            // Addresses are canonical, so an IPv4-mapped range never matches.
            if net.addr().to_canonical() != net.addr() {
                return Err(PatternError::IpRange);
            }
            return Ok(HostPattern::Net(net.trunc()));
        }
        if let Ok(ip) = pattern.parse::<IpAddr>() {
            return Ok(HostPattern::Net(IpNet::from(ip.to_canonical())));
        }
        let (suffix, name) = match pattern.strip_prefix("*.") {
            Some(name) => (true, canonical_name(name)),
            None => (false, canonical_name(pattern)),
        };
        if name.contains('*') {
            return Err(PatternError::Wildcard);
        }
        // The resolver reads names such as `127.1` as addresses, and a
        // guest host never has `%` or non-ASCII text.
        if !is_host_name(&name) {
            return Err(PatternError::Host);
        }
        Ok(if suffix {
            HostPattern::Suffix(name)
        } else {
            HostPattern::Name(name)
        })
    }

    /// Return true if the pattern matches `host`. A name matches only the
    /// name patterns, and an address matches only the ranges. The
    /// localhost aliases (`localhost`, `127.0.0.1`, `::1`) are equal.
    pub fn matches(&self, host: &Host) -> bool {
        let matched = match (self, host) {
            (HostPattern::Any, _) => true,
            (HostPattern::Suffix(suffix), Host::Name(name)) => name
                .strip_suffix(suffix.as_str())
                // The prefix must be at least "x.": a label and a dot.
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
            (HostPattern::Name(pattern), Host::Name(name)) => pattern == name,
            (HostPattern::Net(net), Host::Ip(ip)) => net.contains(ip),
            _ => false,
        };
        matched || (self.is_localhost() && host.is_localhost())
    }

    /// Return true if at least one host matches both patterns.
    pub fn overlaps(&self, other: &HostPattern) -> bool {
        use HostPattern::{Any, Name, Net, Suffix};
        let overlap = match (self, other) {
            (Any, _) | (_, Any) => true,
            // `x.prod.example.com` matches `*.example.com` and
            // `*.prod.example.com`.
            (Suffix(a), Suffix(b)) => {
                a == b
                    || a.strip_suffix(b.as_str()).is_some_and(|p| p.ends_with('.'))
                    || b.strip_suffix(a.as_str()).is_some_and(|p| p.ends_with('.'))
            }
            (Suffix(_), Name(name)) => self.matches(&Host::Name(name.clone())),
            (Name(name), Suffix(_)) => other.matches(&Host::Name(name.clone())),
            (Net(a), Net(b)) => a.contains(&b.network()) || b.contains(&a.network()),
            _ => self == other,
        };
        overlap || (self.is_localhost() && other.is_localhost())
    }

    /// Make a pattern that matches only `host`. The reverse of
    /// [`HostPattern::exact_host`].
    pub fn exact(host: &Host) -> Self {
        match host {
            Host::Name(name) => HostPattern::Name(name.clone()),
            Host::Ip(ip) => HostPattern::Net(IpNet::from(*ip)),
        }
    }

    /// Get the host that this pattern names exactly: a name or a single
    /// address. `None` for `*`, a suffix and a range.
    pub fn exact_host(&self) -> Option<Host> {
        match self {
            HostPattern::Name(name) => Some(Host::Name(name.clone())),
            HostPattern::Net(net) if net.prefix_len() == net.max_prefix_len() => {
                Some(Host::Ip(net.addr()))
            }
            _ => None,
        }
    }

    /// Get the range of the pattern, or `None` if it is not a range.
    pub fn net(&self) -> Option<IpNet> {
        match self {
            HostPattern::Net(net) => Some(*net),
            _ => None,
        }
    }

    /// Get the address ranges that a deny of this pattern covers: the
    /// range of the pattern, and all loopback addresses for `localhost`.
    pub fn deny_ranges(&self) -> Vec<IpNet> {
        let mut ranges: Vec<IpNet> = self.net().into_iter().collect();
        // A name can resolve to any loopback address (Debian host name:
        // `127.0.1.1`). An address pattern stays exact, so an exception
        // for one loopback address still works.
        if matches!(self, HostPattern::Name(name) if name == "localhost") {
            ranges.extend(
                ["127.0.0.0/8", "::1/128"]
                    .iter()
                    .map(|r| r.parse::<IpNet>().expect("valid range")),
            );
        }
        ranges
    }

    fn is_localhost(&self) -> bool {
        match self {
            HostPattern::Name(name) => name == "localhost",
            HostPattern::Net(net) => {
                net.prefix_len() == net.max_prefix_len() && is_localhost_ip(net.addr())
            }
            _ => false,
        }
    }
}

impl fmt::Display for HostPattern {
    /// Write the pattern as a rule writes it. A single address has no
    /// prefix length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostPattern::Any => f.write_str("*"),
            HostPattern::Suffix(suffix) => write!(f, "*.{suffix}"),
            HostPattern::Name(name) => f.write_str(name),
            HostPattern::Net(net) => match self.exact_host() {
                Some(host) => host.fmt(f),
                None => net.fmt(f),
            },
        }
    }
}

/// Parse a valid pattern in tests.
#[cfg(test)]
impl From<&str> for HostPattern {
    fn from(pattern: &str) -> Self {
        Self::parse(pattern).unwrap_or_else(|e| panic!("{pattern}: {e:?}"))
    }
}

/// Why a host pattern is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternError {
    /// A `/` that is not part of a valid IP range.
    IpRange,
    /// A `*` that is not the full host or a `*.` prefix.
    Wildcard,
    /// Neither a name nor an IP address.
    Host,
}

/// Get the canonical form of a host name: lowercase, with no trailing dot
/// (the DNS root label).
pub fn canonical_name(name: &str) -> String {
    // DNS ignores case and reads `host.` as `host`. Without one form,
    // `SECRET.example.com` or `secret.example.com.` would pass a
    // `deny secret.example.com` rule and still reach the denied host.
    name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()
}

fn is_localhost_ip(ip: IpAddr) -> bool {
    ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST)
}

/// Return true if `name` is a valid DNS host name: letters, digits, `-`
/// and `_` in the length limits, and not a numeric address form.
fn is_host_name(name: &str) -> bool {
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    // The URL host parser refuses numeric forms that are not a valid
    // address, for example `0x17f000001`. macOS reads it as `127.0.0.1`.
    name.len() <= 253
        && name.split('.').all(label_ok)
        && matches!(url::Host::parse(name), Ok(url::Host::Domain(_)))
}

#[cfg(test)]
mod tests {
    //! Tests for host parsing and host pattern matching.

    use super::*;

    fn pattern(s: &str) -> HostPattern {
        HostPattern::parse(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    /// Test that host matching follows the wildcard, localhost alias and IP
    /// range rules, and compares addresses, not text. Rules and middleware
    /// select their hosts with this match.
    ///   1. Take hosts and patterns: `*`, `*.suffix`, names, aliases,
    ///      addresses in other text forms, ranges
    ///   2. Check the match result of each pair
    #[test]
    fn host_matching_compares_names_and_addresses() {
        for (host, p, expected) in [
            ("anything.example.com", "*", true),
            ("10.0.0.1", "*", true),
            ("api.example.com", "*.example.com", true),
            ("x.y.z.example.com", "*.example.com", true),
            ("example.com", "*.example.com", false),
            (".example.com", "*.example.com", false),
            ("api.xample.com", "*.example.com", false),
            ("API.EXAMPLE.COM.", "*.example.com", true),
            ("SECRET.example.com.", "secret.example.com", true),
            ("api.example.com", "example.com", false),
            ("127.0.0.1", "localhost", true),
            ("localhost", "127.0.0.1", true),
            ("[::1]", "localhost", true),
            ("localhost", "::1", true),
            // One address in other text forms.
            ("::ffff:169.254.169.254", "169.254.169.254", true),
            ("[::ffff:a9fe:a9fe]", "169.254.169.254", true),
            ("169.254.169.254", "::ffff:169.254.169.254", true),
            ("[FD00::1]", "fd00::1", true),
            ("192.168.1.7", "192.168.1.0/24", true),
            ("192.168.2.7", "192.168.1.0/24", false),
            ("::ffff:192.168.1.7", "192.168.1.0/24", true),
            ("[fd00::1]", "fd00::/8", true),
            ("10.0.0.1", "::/0", false),
            // A name matches only name patterns, an address only ranges.
            ("nas.example.com", "0.0.0.0/0", false),
            ("localhost", "127.0.0.0/8", false),
            // The resolver reads `127.1` as `127.0.0.1`.
            ("127.1", "127.0.0.0/8", true),
            ("127.1", "localhost", true),
        ] {
            assert_eq!(
                pattern(p).matches(&Host::parse(host)),
                expected,
                "{host} ~ {p}"
            );
        }
    }

    /// Test that numeric IPv4 forms parse to the address. As names, they
    /// get past the address rules and still connect to the address.
    ///   1. Take short, decimal, octal, hex and trailing dot forms
    ///   2. Check that each parses to the expected address
    ///   3. Check that near misses stay names
    #[test]
    fn numeric_address_forms_parse_as_addresses() {
        for (host, ip) in [
            ("127.1", "127.0.0.1"),
            ("127.0.1", "127.0.0.1"),
            ("2130706433", "127.0.0.1"),
            ("0x7f.1", "127.0.0.1"),
            ("0x7F000001", "127.0.0.1"),
            ("0177.0.0.1", "127.0.0.1"),
            ("127.0.0.1.", "127.0.0.1"),
            ("169.254.43518", "169.254.169.254"),
            ("0x", "0.0.0.0"),
            ("::1.", "::1"),
        ] {
            assert_eq!(Host::parse(host), Host::Ip(ip.parse().unwrap()), "{host}");
        }
        for host in ["256.1", "1.2.3.4.5", "08", "+1", "0x7g.1", "example.com"] {
            assert!(matches!(Host::parse(host), Host::Name(_)), "{host}");
        }
    }

    /// Test that a host that is neither an IP address nor a valid host name
    /// is not valid. The proxy denies such hosts.
    ///   1. Parse valid names and addresses, and hosts that are not valid
    ///   2. Check the validity of each
    #[test]
    fn host_that_is_not_name_or_address_is_not_valid() {
        for (host, valid) in [
            ("api.example.com", true),
            ("_dmarc.example.com", true),
            ("nas", true),
            ("127.1", true),
            ("[::1]", true),
            ("256.1", false),
            ("08", false),
            ("0x17f000001", false),
            ("0x0.st", true),
            ("cafe.be", true),
            ("fe80::1%en0", false),
            ("a..b", false),
            ("foo bar", false),
            ("", false),
            ("ä.example.com", false),
        ] {
            assert_eq!(Host::parse(host).is_valid(), valid, "{host}");
        }
    }

    /// Test that a pattern that can never match a host is refused. As a
    /// deny, such a pattern would load and deny nothing.
    ///   1. Parse bad ranges, misplaced wildcards and numeric names
    ///   2. Check the error of each
    #[test]
    fn pattern_that_never_matches_is_refused() {
        for (p, error) in [
            ("10.0.0.0/33", PatternError::IpRange),
            ("10.0/8", PatternError::IpRange),
            ("example.com/8", PatternError::IpRange),
            ("::ffff:10.0.0.0/104", PatternError::IpRange),
            ("10.*", PatternError::Wildcard),
            ("*.10.*", PatternError::Wildcard),
            ("*foo.com", PatternError::Wildcard),
            ("127.1", PatternError::Host),
            ("2130706433", PatternError::Host),
            ("*.0.5", PatternError::Host),
            ("fe80::1%en0", PatternError::Host),
            ("127.0x1", PatternError::Host),
            ("0x7f.1", PatternError::Host),
            ("bücher.de", PatternError::Host),
            ("ex%61mple.com", PatternError::Host),
        ] {
            assert_eq!(HostPattern::parse(p), Err(error), "{p}");
        }
    }
}
