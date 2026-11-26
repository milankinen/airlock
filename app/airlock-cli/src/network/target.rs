//! Network targets.
//!
//! Defines the target patterns from the config, the decision for one
//! connection, and the host and port of an upstream. Also includes helpers
//! that classify IP addresses, for example public or private.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::rc::Rc;

use ipnet::IpNet;

use super::host::{Host, HostPattern};
use super::http::middleware::CompiledMiddleware;
use super::interceptor::Interceptor;
use crate::sandbox::MaskedSecret;

/// One `host[:port]` pattern, parsed at startup from the `allow` or `deny`
/// list of a rule.
#[derive(Clone, Debug)]
pub struct NetworkTarget {
    /// Host pattern.
    pub host: HostPattern,
    /// Port, or `None` for all ports.
    pub port: Option<u16>,
}

impl NetworkTarget {
    /// Return true if this target matches `host:port`. An IP address
    /// matches only in its own canonical form. Use it for the targets that
    /// select a connection, for example passthrough or middleware. Use
    /// [`NetworkTarget::allows`] for allow rules and
    /// [`NetworkTarget::denies`] for deny rules.
    pub fn matches(&self, host: &Host, port: u16) -> bool {
        self.host.matches(host) && self.port.is_none_or(|p| p == port)
    }

    /// Return true if this allow rule target allows `host:port`.
    pub fn allows(&self, host: &Host, port: u16) -> bool {
        self.matches(host, port)
    }

    /// Return true if this deny rule target denies `host:port`.
    pub fn denies(&self, host: &Host, port: u16) -> bool {
        match host {
            // An IPv6 address also matches through the IPv4 address that it
            // embeds (NAT64, 6to4). Else that form gets past the deny. Allow
            // rules do not do this: a 6to4 address is a different host.
            Host::Ip(ip) => ip_forms(*ip).any(|ip| self.matches(&Host::Ip(ip), port)),
            Host::Name(_) => self.matches(host, port),
        }
    }
}

/// Host and port that the proxy connects to.
///
/// The host is always canonical. Thus the services can compare endpoints
/// exactly. The guest DNS keeps the case of a name, and `AUTH.OPENAI.COM`
/// or `api.anthropic.com.` is the same host as the endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    host: Host,
    port: u16,
}

impl Endpoint {
    /// Make an endpoint of a host name or an IP address.
    pub fn new(host: &str, port: u16) -> Self {
        Self::of(Host::parse(host), port)
    }

    /// Make an endpoint of a parsed host.
    pub fn of(host: Host, port: u16) -> Self {
        Self { host, port }
    }

    /// Get the canonical host.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// Get the port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the `Host` and `:authority` value for a request to the endpoint:
    /// `host` for port 443, `host:port` for other ports.
    pub fn authority(&self) -> String {
        let host = match &self.host {
            Host::Ip(IpAddr::V6(ip)) => format!("[{ip}]"),
            host => host.to_string(),
        };
        if self.port == 443 {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// Get a target that matches only this endpoint.
    pub fn target(&self) -> NetworkTarget {
        NetworkTarget {
            host: HostPattern::exact(&self.host),
            port: Some(self.port),
        }
    }
}

/// Get the targets of `endpoints`, with no duplicates.
pub fn targets_of(endpoints: &[&Endpoint]) -> Vec<NetworkTarget> {
    let mut out: Vec<NetworkTarget> = Vec::new();
    for e in endpoints {
        let target = e.target();
        if !out
            .iter()
            .any(|t| t.host == target.host && t.port == target.port)
        {
            out.push(target);
        }
    }
    out
}

/// Compiled middleware script with one target pattern.
#[derive(Clone)]
pub struct MiddlewareTarget {
    /// Host pattern.
    pub host: HostPattern,
    /// Port, or `None` for all ports.
    pub port: Option<u16>,
    /// Compiled middleware script.
    pub middleware: CompiledMiddleware,
}

impl MiddlewareTarget {
    /// Return true if this middleware target matches `host:port`.
    pub fn matches(&self, host: &Host, port: u16) -> bool {
        self.host.matches(host) && self.port.is_none_or(|p| p == port)
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
    /// Host pattern.
    pub host: HostPattern,
    /// Port, or `None` for all ports.
    pub port: Option<u16>,
    /// Secrets to inject.
    pub secrets: Vec<InjectedSecret>,
}

impl InjectTarget {
    /// Return true if this inject target matches `host:port`.
    pub fn matches(&self, host: &Host, port: u16) -> bool {
        self.host.matches(host) && self.port.is_none_or(|p| p == port)
    }
}

/// Decision for one connection: if it is allowed, and how the proxy
/// handles it.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)] // independent flags
pub struct ResolvedTarget {
    /// Destination host (after port forward changes).
    pub host: Host,
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
    /// True if the connection is denied only because no allow rule
    /// matches under `deny-by-default`. A temporary grant can allow such a
    /// target. False for allowed targets and for all other denies.
    pub grantable: bool,
    /// If true, skip TLS and HTTP interception and relay the connection as
    /// plain TCP. Set for targets that match a passthrough rule and for
    /// localhost port-forwarded destinations. These may use non-HTTP
    /// protocols, and a sniff of their first bytes can cause a deadlock.
    pub passthrough: bool,
    /// If true, connect only to public addresses ([`is_public_ip`]). The
    /// proxy ignores all other addresses that the host resolves to. Thus a
    /// name cannot reach the host loopback, the LAN or cloud metadata.
    pub public_only: bool,
    /// Address ranges of the deny rules. The proxy ignores the addresses in
    /// them that the host resolves to.
    pub denied_ranges: Vec<IpNet>,
    /// Addresses to connect to. Empty until the relay resolves a name that
    /// needs an address check (see [`super::tcp::resolve`]).
    pub addresses: Vec<SocketAddr>,
    /// Hosts of all network services. A request on a connection that no
    /// service owns must not name one of them in its `Host` or
    /// `:authority`.
    pub service_hosts: Rc<[Host]>,
}

impl ResolvedTarget {
    /// Return true if the connection is allowed and must skip all
    /// interception, and the proxy relays it as plain TCP. Denied
    /// connections never use passthrough. They must get to the 403 code
    /// path.
    pub fn is_passthrough(&self) -> bool {
        self.allowed && self.passthrough
    }

    /// Return true if the proxy can connect to each address of the host.
    pub fn allows_all_addresses(&self) -> bool {
        !self.public_only && self.denied_ranges.is_empty()
    }

    /// Return true if the proxy can connect to the address `ip`.
    pub fn allows_address(&self, ip: IpAddr) -> bool {
        (!self.public_only || is_public_ip(ip)) && !in_ranges(&self.denied_ranges, ip)
    }

    /// Deny the connection after the decision, for example because the
    /// host resolves to no allowed address. The connection then gets the
    /// normal deny handling.
    pub fn deny(&mut self) {
        self.allowed = false;
        self.grantable = false;
        self.passthrough = false;
        self.interceptor = None;
        self.middleware.clear();
        self.secrets.clear();
    }
}

/// Get the IP ranges that the deny `targets` deny on `port`
/// ([`HostPattern::deny_ranges`]).
pub fn ip_ranges(targets: &[NetworkTarget], port: u16) -> Vec<IpNet> {
    targets
        .iter()
        .filter(|t| t.port.is_none_or(|p| p == port))
        .flat_map(|t| t.host.deny_ranges())
        .collect()
}

/// Return true if `ip` is in one of the `ranges`. The IPv4 address that an
/// IPv6 address embeds counts too ([`embedded_v4`]). Else the IPv6 form of
/// an address gets past a range.
pub fn in_ranges(ranges: &[IpNet], ip: IpAddr) -> bool {
    ip_forms(ip).any(|ip| ranges.iter().any(|r| r.contains(&ip)))
}

/// Get `ip` in canonical form, and the IPv4 address that it embeds.
fn ip_forms(ip: IpAddr) -> impl Iterator<Item = IpAddr> {
    let ip = ip.to_canonical();
    std::iter::once(ip).chain(embedded_v4(ip).map(IpAddr::V4))
}

/// Ranges of the addresses that are not public: unspecified, loopback,
/// private (RFC 1918, ULA), link-local (cloud metadata), shared (CGNAT),
/// broadcast, multicast and reserved. The private network pack denies the
/// same ranges.
pub static PRIVATE_RANGES: std::sync::LazyLock<Vec<IpNet>> = std::sync::LazyLock::new(|| {
    [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "::/128",
        "::1/128",
        "fc00::/7",
        "fe80::/10",
        "fec0::/10",
        "ff00::/8",
    ]
    .iter()
    .map(|r| r.parse().expect("valid range"))
    .collect()
});

/// Return true if `ip` is a public address: not in [`PRIVATE_RANGES`].
/// For IPv4-compatible, IPv4-mapped, NAT64 and 6to4 addresses, the
/// embedded IPv4 address decides.
pub fn is_public_ip(ip: IpAddr) -> bool {
    !in_ranges(&PRIVATE_RANGES, ip)
}

/// Get the IPv4 address in an IPv4-compatible (deprecated), IPv4-mapped,
/// IPv4-translated, NAT64 or 6to4 address.
fn embedded_v4(ip: IpAddr) -> Option<Ipv4Addr> {
    let IpAddr::V6(ip) = ip else { return None };
    match ip.segments() {
        // `::` and `::1` are IPv6 addresses, not IPv4-compatible ones.
        [0, 0, 0, 0, 0, 0, 0, 0 | 1] => None,
        [0, 0, 0, 0, 0, 0 | 0xffff, hi, lo]
        | [0, 0, 0, 0, 0xffff, 0, hi, lo]
        | [0x64, 0xff9b, 0, 0, 0, 0, hi, lo]
        // Local-use NAT64 (RFC 8215): only the usual /96 form.
        | [0x64, 0xff9b, 1, _, _, _, hi, lo]
        | [0x2002, hi, lo, ..] => Some(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))),
        _ => None,
    }
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
            "64:ff9b:1::a9fe:a9fe",
            "::ffff:0:a9fe:a9fe",
            "::127.0.0.1",
            "::10.0.0.1",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::",
            "2002:c0a8:101::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    /// Test that an IPv6 address matches an IPv4 range only through a real
    /// embedded IPv4 address. Else a deny of `0.0.0.0/8` also denies `::1`
    /// and `::`.
    ///   1. Take IPv6 addresses with and without an embedded IPv4 address
    ///   2. Check each against IPv4 ranges
    #[test]
    fn ipv6_address_matches_ipv4_range_only_through_embedded_address() {
        let ranges: Vec<IpNet> = ["0.0.0.0/8", "169.254.0.0/16"]
            .iter()
            .map(|r| r.parse().unwrap())
            .collect();
        for (ip, expected) in [
            ("::1", false),
            ("::", false),
            ("::0.0.0.2", true),
            ("::ffff:0:a9fe:a9fe", true),
            ("64:ff9b:1:2::a9fe:a9fe", true),
            ("64:ff9b:2::a9fe:a9fe", false),
        ] {
            assert_eq!(in_ranges(&ranges, ip.parse().unwrap()), expected, "{ip}");
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
            "64:ff9b:1::808:808",
            "::8.8.8.8",
            "2002:808:808::1",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
