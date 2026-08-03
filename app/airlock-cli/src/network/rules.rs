use super::http;
use super::middleware::LogFn;
use super::target::{MiddlewareTarget, NetworkTarget};
use crate::config::config::Network;
use crate::vault::Vault;

/// Rule targets extracted from enabled rules.
///
/// `passthrough` is a subset of `allow`: entries from rules with
/// `passthrough = true`. They're kept separate so the connect path can decide
/// whether to short-circuit interception without re-scanning rule metadata.
pub struct RuleTargets {
    pub allow: Vec<NetworkTarget>,
    pub deny: Vec<NetworkTarget>,
    pub passthrough: Vec<NetworkTarget>,
}

/// Resolve config rules into allow/deny/passthrough target lists.
/// Disabled rules are skipped.
pub fn resolve(network: &Network) -> RuleTargets {
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    let mut passthrough = Vec::new();

    for rule in network.rules.values() {
        if !rule.enabled {
            continue;
        }

        for target_str in &rule.allow {
            let (host, port) = parse_target(target_str);
            let target = NetworkTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
            };
            if rule.passthrough {
                passthrough.push(target.clone());
            }
            allow.push(target);
        }

        for target_str in &rule.deny {
            let (host, port) = parse_target(target_str);
            deny.push(NetworkTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
            });
        }
    }

    RuleTargets {
        allow,
        deny,
        passthrough,
    }
}

/// Compile middleware from the `network.middleware` config section.
/// Each enabled middleware rule is compiled and paired with its target patterns.
pub fn resolve_middleware(
    network: &Network,
    vault: &Vault,
    log: &LogFn,
) -> anyhow::Result<Vec<MiddlewareTarget>> {
    let mut targets = Vec::new();

    for mw in network.middleware.values() {
        if !mw.enabled {
            continue;
        }

        let compiled = http::middleware::compile(&mw.script, &mw.env, vault, log.clone())?;

        for target_str in &mw.target {
            let (host, port) = parse_target(target_str);
            targets.push(MiddlewareTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
                middleware: compiled.clone(),
            });
        }
    }

    Ok(targets)
}

/// Derive guest → host port forward mappings from config.
/// Returns `(guest_port, host_port)` pairs from all enabled port forward groups.
pub fn port_forwards_from_config(network: &Network) -> Vec<(u16, u16)> {
    let mut forwards = Vec::new();
    for pf in network.ports.values() {
        if !pf.enabled {
            continue;
        }
        for mapping in &pf.host {
            let pair = (mapping.guest, mapping.host);
            if !forwards.contains(&pair) {
                forwards.push(pair);
            }
        }
    }
    forwards
}

/// Derive host → guest port forward mappings from config.
/// Returns `(host_port, guest_port)` pairs from all enabled port forward
/// groups — the host binds `127.0.0.1:<host_port>` and each connection
/// is bridged into the guest on `127.0.0.1:<guest_port>`.
pub fn reverse_port_forwards_from_config(network: &Network) -> Vec<(u16, u16)> {
    let mut forwards = Vec::new();
    for pf in network.ports.values() {
        if !pf.enabled {
            continue;
        }
        for mapping in &pf.guest {
            let pair = (mapping.host, mapping.guest);
            if !forwards.contains(&pair) {
                forwards.push(pair);
            }
        }
    }
    forwards
}

/// Parse a target pattern `host[:port]` into (host, port_str).
///
/// Handles IPv6 literals, which a naive `rsplit_once(':')` mangles (`::1`
/// would parse to host `":"`, port `"1"`, matching nothing):
/// - `[::1]` / `[::1]:443` — bracketed form, port after `]`.
/// - `2001:db8::1` — a bare IPv6 literal (more than one colon) has no
///   unbracketed `:port` form, so the whole string is the host.
/// - everything else — a hostname or IPv4 with an optional `:port`.
pub(super) fn parse_target(target: &str) -> (&str, Option<&str>) {
    if let Some(rest) = target.strip_prefix('[') {
        // Bracketed IPv6 literal.
        return match rest.split_once(']') {
            Some((host, after)) => (host, after.strip_prefix(':').filter(|p| !p.is_empty())),
            None => (target, None), // malformed; treat whole as host
        };
    }
    if target.matches(':').count() > 1 {
        // Bare IPv6 literal — no port.
        return (target, None);
    }
    match target.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (target, None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::config::{self, NetworkRule, Policy};

    fn rule(allow: &[&str], passthrough: bool) -> NetworkRule {
        NetworkRule {
            enabled: true,
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: vec![],
            passthrough,
        }
    }

    #[test]
    fn parse_target_handles_hostnames_and_ipv4() {
        assert_eq!(parse_target("api.example.com"), ("api.example.com", None));
        assert_eq!(
            parse_target("api.example.com:443"),
            ("api.example.com", Some("443"))
        );
        assert_eq!(parse_target("10.0.0.1:80"), ("10.0.0.1", Some("80")));
    }

    #[test]
    fn parse_target_handles_ipv6_literals() {
        // Bare IPv6 → host only, no port (the bug: rsplit would give (":","1")).
        assert_eq!(parse_target("::1"), ("::1", None));
        assert_eq!(parse_target("2001:db8::1"), ("2001:db8::1", None));
        // Bracketed forms carry the port after the closing bracket.
        assert_eq!(parse_target("[::1]"), ("::1", None));
        assert_eq!(parse_target("[::1]:443"), ("::1", Some("443")));
        assert_eq!(
            parse_target("[2001:db8::1]:8080"),
            ("2001:db8::1", Some("8080"))
        );
    }

    #[test]
    fn passthrough_rule_contributes_to_both_allow_and_passthrough() {
        let mut rules = BTreeMap::new();
        rules.insert("pt".to_string(), rule(&["db.example.com:5432"], true));
        rules.insert("plain".to_string(), rule(&["api.example.com"], false));
        let net = config::Network {
            policy: Policy::DenyByDefault,
            rules,
            middleware: BTreeMap::default(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
        };
        let resolved = resolve(&net);
        assert_eq!(resolved.allow.len(), 2);
        assert_eq!(resolved.passthrough.len(), 1);
        assert!(resolved.passthrough[0].matches("db.example.com", 5432));
        assert!(!resolved.passthrough[0].matches("api.example.com", 443));
    }

    #[test]
    fn disabled_passthrough_rule_is_skipped() {
        let mut rules = BTreeMap::new();
        rules.insert(
            "pt".to_string(),
            NetworkRule {
                enabled: false,
                allow: vec!["db.example.com".to_string()],
                deny: vec![],
                passthrough: true,
            },
        );
        let net = config::Network {
            policy: Policy::DenyByDefault,
            rules,
            middleware: BTreeMap::default(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
        };
        let resolved = resolve(&net);
        assert!(resolved.allow.is_empty());
        assert!(resolved.passthrough.is_empty());
    }
}
