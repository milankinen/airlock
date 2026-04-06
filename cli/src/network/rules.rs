use super::http;
use super::middleware::LogFn;
use super::target::NetworkTarget;
use crate::config::config::Network;

/// Resolve config rules into a flat list of network targets with
/// compiled middleware attached.
pub fn resolve(network: &Network, log: &LogFn) -> anyhow::Result<Vec<NetworkTarget>> {
    let mut targets = Vec::new();

    for rule in &network.rules {
        for allow in &rule.allow {
            let (host, port) = parse_target(allow);
            let port = port.and_then(|p| p.parse::<u16>().ok());

            // Collect middleware scripts that match this host
            let mut middleware = Vec::new();
            for (mw_host, scripts) in &rule.middleware {
                if mw_host == host || mw_host == "*" {
                    for (i, mw) in scripts.iter().enumerate() {
                        let name = if scripts.len() == 1 {
                            format!("{}:{}:{}", rule.name, host, mw_host)
                        } else {
                            format!("{}:{}:{}[{i}]", rule.name, host, mw_host)
                        };
                        middleware.push(http::middleware::compile(&name, &mw.script, log.clone())?);
                    }
                }
            }

            targets.push(NetworkTarget {
                host: host.to_string(),
                port,
                middleware,
            });
        }
    }

    Ok(targets)
}

/// Derive localhost ports directly from config (no compilation needed).
pub fn localhost_ports_from_config(network: &Network) -> Vec<u16> {
    let mut ports = Vec::new();
    for rule in &network.rules {
        for target in &rule.allow {
            let (host, port) = parse_target(target);
            if is_localhost(host)
                && let Some(port_str) = port
                && let Ok(p) = port_str.parse::<u16>()
                && !ports.contains(&p)
            {
                ports.push(p);
            }
        }
    }
    ports
}

/// Derive TLS passthrough hosts directly from config (no compilation needed).
/// A host gets passthrough if it has no middleware in any rule.
pub fn tls_passthrough_from_config(network: &Network) -> Vec<String> {
    let mut passthrough = Vec::new();
    let mut has_middleware = std::collections::HashSet::new();
    for rule in &network.rules {
        for host_pattern in rule.middleware.keys() {
            has_middleware.insert(host_pattern.clone());
        }
    }
    for rule in &network.rules {
        for target in &rule.allow {
            let (host, _) = parse_target(target);
            if !is_localhost(host)
                && !has_middleware.contains(host)
                && !passthrough.contains(&host.to_string())
            {
                passthrough.push(host.to_string());
            }
        }
    }
    passthrough
}

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Parse a target pattern into (host, port_str). Missing port → None.
fn parse_target(target: &str) -> (&str, Option<&str>) {
    match target.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (target, None),
    }
}
