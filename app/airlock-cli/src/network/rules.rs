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

/// Derive port forward mappings from config.
/// For `network.ports` (host→guest): source = host port, target = guest port.
/// Returns `(guest_port, host_port)` pairs from all enabled port forward groups.
pub fn port_forwards_from_config(network: &Network) -> Vec<(u16, u16)> {
    let mut forwards = Vec::new();
    for pf in network.ports.values() {
        if !pf.enabled {
            continue;
        }
        for mapping in &pf.host {
            let pair = (mapping.target, mapping.source);
            if !forwards.contains(&pair) {
                forwards.push(pair);
            }
        }
    }
    forwards
}

/// Parse a target pattern `host[:port]` into (host, port_str).
pub(super) fn parse_target(target: &str) -> (&str, Option<&str>) {
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
