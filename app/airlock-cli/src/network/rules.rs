use super::http;
use super::middleware::LogFn;
use super::target::{MiddlewareTarget, NetworkTarget};
use crate::config::config::Network;
use crate::vault::Vault;

/// Resolve config rules into allow and deny target lists.
/// Disabled rules are skipped.
///
/// Returns `(allow_targets, deny_targets)`.
pub fn resolve(network: &Network) -> (Vec<NetworkTarget>, Vec<NetworkTarget>) {
    let mut allow_targets = Vec::new();
    let mut deny_targets = Vec::new();

    for rule in network.rules.values() {
        if !rule.enabled {
            continue;
        }

        for target_str in &rule.allow {
            let (host, port) = parse_target(target_str);
            allow_targets.push(NetworkTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
            });
        }

        for target_str in &rule.deny {
            let (host, port) = parse_target(target_str);
            deny_targets.push(NetworkTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
            });
        }
    }

    (allow_targets, deny_targets)
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
fn parse_target(target: &str) -> (&str, Option<&str>) {
    match target.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (target, None),
    }
}
