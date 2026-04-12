use super::http;
use super::middleware::LogFn;
use super::target::NetworkTarget;
use crate::config::config::Network;

/// Resolve config rules into a flat list of network targets with
/// compiled middleware attached. Disabled rules/middleware are skipped.
pub fn resolve(network: &Network, log: &LogFn) -> anyhow::Result<Vec<NetworkTarget>> {
    let mut targets = Vec::new();

    for rule in network.rules.values() {
        if !rule.enabled {
            continue;
        }
        for allow in &rule.allow {
            let (http_only, host, port) = parse_target(allow);
            let port = port.and_then(|p| p.parse::<u16>().ok());

            // Collect enabled middleware scripts that match this host
            let mut middleware = Vec::new();
            for mw in &rule.middleware {
                middleware.push(http::middleware::compile(&mw.script, &mw.env, log.clone())?);
            }

            targets.push(NetworkTarget {
                host: host.to_string(),
                port,
                http_only,
                middleware,
            });
        }
    }

    Ok(targets)
}

/// Derive localhost ports directly from config (no compilation needed).
pub fn localhost_ports_from_config(network: &Network) -> Vec<u16> {
    let mut ports = Vec::new();
    for rule in network.rules.values() {
        if !rule.enabled {
            continue;
        }
        for target in &rule.allow {
            let (_, host, port) = parse_target(target);
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

fn is_localhost(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Parse a target pattern `[http:]host[:port]` into (http_only, host, port_str).
fn parse_target(target: &str) -> (bool, &str, Option<&str>) {
    let (http_only, rest) = match target.strip_prefix("http:") {
        Some(rest) => (true, rest),
        None => (false, target),
    };
    match rest.rsplit_once(':') {
        Some((host, port)) => (http_only, host, Some(port)),
        None => (http_only, rest, None),
    }
}
