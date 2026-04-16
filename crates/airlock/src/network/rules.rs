use super::http;
use super::middleware::LogFn;
use super::target::NetworkTarget;
use crate::config::config::Network;

/// Resolve config rules into allow and deny target lists with compiled
/// middleware attached. Disabled rules are skipped.
///
/// Returns `(allow_targets, deny_targets)`.
pub fn resolve(
    network: &Network,
    log: &LogFn,
) -> anyhow::Result<(Vec<NetworkTarget>, Vec<NetworkTarget>)> {
    let mut allow_targets = Vec::new();
    let mut deny_targets = Vec::new();

    for rule in network.rules.values() {
        if !rule.enabled {
            continue;
        }

        let compiled_middleware: Vec<_> = rule
            .middleware
            .iter()
            .map(|mw| http::middleware::compile(&mw.script, &mw.env, log.clone()))
            .collect::<anyhow::Result<_>>()?;

        for target_str in &rule.allow {
            let (host, port) = parse_target(target_str);
            allow_targets.push(NetworkTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
                middleware: compiled_middleware.clone(),
            });
        }

        for target_str in &rule.deny {
            let (host, port) = parse_target(target_str);
            deny_targets.push(NetworkTarget {
                host: host.to_string(),
                port: port.and_then(|p| p.parse::<u16>().ok()),
                middleware: vec![],
            });
        }
    }

    Ok((allow_targets, deny_targets))
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
