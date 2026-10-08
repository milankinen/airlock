//! Network targets.
//!
//! Defines the target patterns from the config, the decision for one
//! connection, and the host and port of an upstream. Also includes helpers
//! that classify IP addresses, for example public or private.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;

use super::http::middleware::CompiledMiddleware;
use super::interceptor::Interceptor;
use super::matchers;
use crate::project::MaskedSecret;

/// One `host[:port]` pattern, parsed at startup from the `allow` or `deny`
/// list of a rule.
#[derive(Clone, Debug)]
pub struct NetworkTarget {
    /// Host pattern (see [`matchers::host_matches`]).
    pub host: String,
    /// Port, or `None` for all ports.
    pub port: Option<u16>,
}

impl NetworkTarget {
    /// Return true if this target matches `host:port`.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// Host and port that the proxy connects to.
///
/// The host is always canonical ([`matchers::canonical_host`]: lowercase,
/// no trailing dot). Thus the services can compare endpoints exactly. The
/// guest DNS keeps the case of a name, and `AUTH.OPENAI.COM` or
/// `api.anthropic.com.` is the same host as the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    /// Make an endpoint. The host becomes canonical.
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: matchers::canonical_host(host),
            port,
        }
    }

    /// Get the canonical host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the `Host` and `:authority` value for a request to the endpoint:
    /// `host` for port 443, `host:port` for other ports.
    pub fn authority(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Get a target that matches only this endpoint.
    pub fn target(&self) -> NetworkTarget {
        NetworkTarget {
            host: self.host.clone(),
            port: Some(self.port),
        }
    }
}

/// Get the targets of `endpoints`, with no duplicates.
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

/// Compiled middleware script with one target pattern.
#[derive(Clone)]
pub struct MiddlewareTarget {
    /// Host pattern (see [`matchers::host_matches`]).
    pub host: String,
    /// Port, or `None` for all ports.
    pub port: Option<u16>,
    /// Compiled middleware script.
    pub middleware: CompiledMiddleware,
}

impl MiddlewareTarget {
    /// Return true if this middleware target matches `host:port`.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// Masked secret as the proxy keeps it. The secret is shared in an `Rc`,
/// so a copy for each connection and request clones only a pointer, not
/// strings. Derefs to the [`MaskedSecret`].
#[derive(Clone, Debug)]
pub struct InjectedSecret(Rc<MaskedSecret>);

impl InjectedSecret {
    /// Make a shared handle for `secret`.
    pub fn new(secret: MaskedSecret) -> Self {
        Self(Rc::new(secret))
    }
}

impl std::ops::Deref for InjectedSecret {
    type Target = MaskedSecret;

    fn deref(&self) -> &MaskedSecret {
        &self.0
    }
}

/// Masked secrets that a rule injects into HTTP headers, with one allow
/// pattern of the rule. There is one entry for each `(rule, allow pattern)`.
/// All entries of a rule share the same secrets.
#[derive(Clone, Debug)]
pub struct InjectTarget {
    /// Host pattern (see [`matchers::host_matches`]).
    pub host: String,
    /// Port, or `None` for all ports.
    pub port: Option<u16>,
    /// Secrets to inject.
    pub secrets: Vec<InjectedSecret>,
}

impl InjectTarget {
    /// Return true if this inject target matches `host:port`.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        matchers::host_matches(host, &self.host) && self.port.is_none_or(|p| p == port)
    }
}

/// Decision for one connection: if it is allowed, and how the proxy
/// handles it.
#[derive(Clone)]
pub struct ResolvedTarget {
    /// Destination host (after port forward changes).
    pub host: String,
    /// Destination port (after port forward changes).
    pub port: u16,
    /// Middleware scripts from all matching middleware rules.
    pub middleware: Vec<CompiledMiddleware>,
    /// Masked secrets to replace in request headers (to the real value) and
    /// in response headers (to the masked value). They come from all
    /// matching rules with `inject`. Empty for denied and passthrough
    /// connections.
    pub secrets: Vec<InjectedSecret>,
    /// Interceptor that owns the target. It handles each request on the
    /// connection around the upstream send. `None` for denied connections.
    pub interceptor: Option<Rc<dyn Interceptor>>,
    /// True if the connection is allowed. False if the policy or a deny
    /// rule denies it, or if no allow rule matches.
    pub allowed: bool,
    /// If true, skip TLS and HTTP interception and relay the connection as
    /// plain TCP. Set for targets that match a passthrough rule and for
    /// localhost port-forwarded destinations. These may use non-HTTP
    /// protocols, and a sniff of their first bytes can cause a deadlock.
    pub passthrough: bool,
    /// If true, connect only to public addresses ([`is_public_ip`]). The
    /// proxy ignores all other addresses that the host resolves to. Thus a
    /// name cannot reach the host loopback, the LAN or cloud metadata.
    pub public_only: bool,
}

impl ResolvedTarget {
    /// Return true if the connection is allowed and must skip all
    /// interception, and the proxy relays it as plain TCP. Denied
    /// connections never use passthrough. They must get to the 403 code
    /// path.
    pub fn is_passthrough(&self) -> bool {
        self.allowed && self.passthrough
    }
}

/// Return true if `ip` is a public address. A public address is not
/// unspecified, loopback, private (RFC 1918, ULA), link-local (cloud
/// metadata), shared (CGNAT), broadcast, multicast or reserved.
/// For IPv4-compatible, IPv4-mapped, NAT64 and 6to4 addresses, the
/// embedded IPv4 address decides.
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

/// Parse `host` as an IP literal, with or without brackets.
/// Returns:
///   The IP address, or `None` if `host` is not an IP literal.
pub fn ip_literal(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    //! Tests for the public address check of the public-only network.

    use super::*;

    /// Test that local, private, metadata and other special addresses are
    /// not public. A public-only network uses this check, so a miss lets the
    /// guest reach the host or the LAN.
    ///   1. Take IPv4 and IPv6 special addresses, also IPv4 in IPv6 forms
    ///   2. Check that none of them is public
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
            // IPv4-mapped, NAT64, IPv4-compatible and 6to4 forms of
            // loopback, metadata and private IPv4 addresses.
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

    /// Test that internet addresses are public, also near the edge of the
    /// private ranges and in IPv4 in IPv6 forms.
    ///   1. Take public IPv4 and IPv6 addresses
    ///   2. Check that each of them is public
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
}
