use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;

use super::http::middleware::CompiledMiddleware;
use super::interceptor::Interceptor;
use super::matchers;
use crate::project::MaskedSecret;

/// A resolved network target — parsed from a rule's `allow` or `deny` list
/// at startup. Each target represents one `host[:port]` pattern.
#[derive(Clone, Debug)]
pub struct NetworkTarget {
    pub host: String,
    pub port: Option<u16>,
}

impl NetworkTarget {
    /// Does this target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// A host and port the proxy talks to. The host is always canonical
/// ([`matchers::canonical_host`]: lowercase, no trailing dot), so the
/// services compare endpoints exactly: the guest's DNS keeps the case of
/// a name, and `AUTH.OPENAI.COM` or `api.anthropic.com.` is the same host
/// as the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: matchers::canonical_host(host),
            port,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `host`, or `host:port` for a port other than 443: the `Host` and
    /// `:authority` of a request to the endpoint.
    pub fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn target(&self) -> NetworkTarget {
        NetworkTarget {
            host: self.host.clone(),
            port: Some(self.port),
        }
    }
}

/// The targets of `endpoints`, without duplicates.
pub fn targets_of(endpoints: &[&Endpoint]) -> Vec<NetworkTarget> {
    let mut out: Vec<NetworkTarget> = Vec::new();
    for e in endpoints {
        if !out
            .iter()
            .any(|t| t.host == e.host && t.port == Some(e.port))
        {
            out.push(e.target());
        }
    }
    out
}

/// A compiled middleware script with target patterns for matching.
#[derive(Clone)]
pub struct MiddlewareTarget {
    pub host: String,
    pub port: Option<u16>,
    pub middleware: CompiledMiddleware,
}

impl MiddlewareTarget {
    /// Does this middleware target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// A masked secret as the proxy holds it: shared behind an `Rc` so the
/// per-connection and per-request copies are pointer bumps, not string
/// clones. Derefs to the underlying [`MaskedSecret`].
#[derive(Clone, Debug)]
pub struct InjectedSecret(Rc<MaskedSecret>);

impl InjectedSecret {
    pub fn new(secret: MaskedSecret) -> Self {
        Self(Rc::new(secret))
    }

    /// Whether two handles share the same underlying secret allocation.
    #[cfg(test)]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }
}

impl std::ops::Deref for InjectedSecret {
    type Target = MaskedSecret;

    fn deref(&self) -> &MaskedSecret {
        &self.0
    }
}

/// Masked secrets a rule injects into HTTP headers, paired with one of the
/// rule's allow patterns. One entry per `(rule, allow pattern)`; all entries
/// of a rule share the same underlying secrets.
#[derive(Clone, Debug)]
pub struct InjectTarget {
    pub host: String,
    pub port: Option<u16>,
    pub secrets: Vec<InjectedSecret>,
}

impl InjectTarget {
    /// Does this inject target match the given host:port?
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

#[derive(Clone)]
pub struct ResolvedTarget {
    pub host: String,
    pub port: u16,
    /// Middleware scripts from all matching middleware rules.
    pub middleware: Vec<CompiledMiddleware>,
    /// Masked secrets to swap in outbound request headers / out of
    /// response headers, from all matching rules with `inject`. Empty for
    /// denied and passthrough connections.
    pub secrets: Vec<InjectedSecret>,
    /// The interceptor that owns the target; it handles every request on
    /// the connection around the upstream send. `None` for denied
    /// connections.
    pub interceptor: Option<Rc<dyn Interceptor>>,
    /// Whether this connection is permitted.
    /// False if denied by policy, deny rule, or no allow rule matched.
    pub allowed: bool,
    /// Skip TLS/HTTP interception and relay the connection as plain TCP.
    /// Set for targets matching a passthrough rule and for localhost
    /// port-forwarded destinations (which may carry non-HTTP protocols
    /// whose first bytes can't be sniffed without deadlocking).
    pub passthrough: bool,
    /// Connect only to public addresses ([`is_public_ip`]): the proxy
    /// drops every other address the host resolves to, so a name cannot
    /// reach the host's loopback, the LAN or cloud metadata.
    pub public_only: bool,
}

impl ResolvedTarget {
    /// True when the allowed connection should skip all interception and
    /// be relayed as plain TCP. Denied connections never passthrough —
    /// they still need to reach the 403 code path.
    pub fn is_passthrough(&self) -> bool {
        self.allowed && self.passthrough
    }
}

/// Whether `ip` is a public address: not unspecified, loopback, private
/// (RFC 1918, ULA), link-local (cloud metadata), shared (CGNAT),
/// broadcast, multicast or reserved. IPv4-compatible, IPv4-mapped,
/// NAT64 and 6to4 addresses are judged by their IPv4 address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (a == 100 && (b & 0xc0) == 64)
        || ip.is_broadcast()
        || ip.is_multicast()
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() {
        return false;
    }
    let seg = ip.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    // IPv4-compatible (deprecated), IPv4-mapped, NAT64 and 6to4.
    let embedded = match seg {
        [0, 0, 0, 0, 0, 0 | 0xffff, hi, lo]
        | [0x64, 0xff9b, 0, 0, 0, 0, hi, lo]
        | [0x2002, hi, lo, ..] => Some(v4(hi, lo)),
        _ => None,
    };
    if let Some(v4) = embedded {
        return is_public_v4(v4);
    }
    !(ip.is_unique_local()
        || ip.is_unicast_link_local()
        // Site-local (deprecated) and multicast.
        || (seg[0] & 0xffc0) == 0xfec0
        || ip.is_multicast())
}

/// `host` as an IP address: a literal, bare or in brackets.
pub fn ip_literal(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_private_and_metadata_addresses_are_not_public() {
        for ip in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.100.100.200",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "::127.0.0.1",
            "::10.0.0.1",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::",
            "2002:c0a8:101::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn internet_addresses_are_public() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "169.255.0.1",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::808:808",
            "::8.8.8.8",
            "2002:808:808::1",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn ip_literal_takes_bare_and_bracketed_addresses() {
        assert_eq!(ip_literal("127.0.0.1"), Some(IpAddr::from([127, 0, 0, 1])));
        assert_eq!(ip_literal("[::1]"), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(ip_literal("::1"), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(ip_literal("localhost"), None);
    }
}
