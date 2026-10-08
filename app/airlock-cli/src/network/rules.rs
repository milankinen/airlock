use anyhow::Context;

use super::http;
use super::middleware::LogFn;
use super::target::{InjectTarget, InjectedSecret, MiddlewareTarget, NetworkTarget};
use crate::config::config_values::Network;
use crate::project::SandboxEnv;
use crate::vault::Vault;

/// Rule targets extracted from enabled rules.
///
/// `passthrough` is a subset of `allow`: entries from rules with
/// `passthrough = true`. They're kept separate so the connect path can decide
/// whether to short-circuit interception without re-scanning rule metadata.
#[derive(Debug)]
pub struct RuleTargets {
    pub allow: Vec<NetworkTarget>,
    pub deny: Vec<NetworkTarget>,
    pub passthrough: Vec<NetworkTarget>,
}

/// Resolve config rules into allow/deny/passthrough target lists.
/// Disabled rules are skipped. Errors on a malformed pattern (see
/// [`parse_pattern`]); config loading reports the same problems earlier
/// with the offending rule named, so this is the fail-closed backstop.
pub fn resolve(network: &Network) -> anyhow::Result<RuleTargets> {
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    let mut passthrough = Vec::new();

    for (rule_name, rule) in &network.rules {
        if !rule.enabled {
            continue;
        }

        for target_str in &rule.allow {
            let (host, port) = parse_pattern(target_str)
                .with_context(|| format!("network.rules.{rule_name}.allow"))?;
            let target = NetworkTarget {
                host: host.to_string(),
                port,
            };
            if rule.passthrough {
                passthrough.push(target.clone());
            }
            allow.push(target);
        }

        for target_str in &rule.deny {
            let (host, port) = parse_pattern(target_str)
                .with_context(|| format!("network.rules.{rule_name}.deny"))?;
            deny.push(NetworkTarget {
                host: host.to_string(),
                port,
            });
        }
    }

    Ok(RuleTargets {
        allow,
        deny,
        passthrough,
    })
}

/// Compile middleware from the `network.middleware` config section.
/// Each enabled middleware rule is compiled and paired with its target patterns.
pub fn resolve_middleware(
    network: &Network,
    vault: &Vault,
    log: &LogFn,
) -> anyhow::Result<Vec<MiddlewareTarget>> {
    let mut targets = Vec::new();

    for (mw_name, mw) in &network.middleware {
        if !mw.enabled {
            continue;
        }

        let compiled = http::middleware::compile(&mw.script, &mw.env, vault, log.clone())?;

        for target_str in &mw.target {
            let (host, port) = parse_pattern(target_str)
                .with_context(|| format!("network.middleware.{mw_name}.target"))?;
            targets.push(MiddlewareTarget {
                host: host.to_string(),
                port,
                middleware: compiled.clone(),
            });
        }
    }

    Ok(targets)
}

/// Resolve `inject` lists from enabled rules into targets carrying the
/// masked secrets. Config loading guarantees every name is a masked `[env]`
/// entry and that no injecting rule is `passthrough`; `project::open` /
/// [`crate::project::Project::with_config`] check the values are
/// injectable. This still errors (rather than panics) on a missing entry.
pub fn resolve_inject(network: &Network, env: &SandboxEnv) -> anyhow::Result<Vec<InjectTarget>> {
    let mut targets = Vec::new();

    for (rule_name, rule) in &network.rules {
        if !rule.enabled || rule.inject.is_empty() {
            continue;
        }

        let mut secrets: Vec<InjectedSecret> = Vec::with_capacity(rule.inject.len());
        for name in &rule.inject {
            let Some(secret) = env.masked(name) else {
                anyhow::bail!(
                    "network.rules.{rule_name}.inject: `{name}` must be defined in [env] with mask = true"
                );
            };
            if !secrets.iter().any(|s| s.name == *name) {
                secrets.push(InjectedSecret::new(secret.clone()));
            }
        }

        for target_str in &rule.allow {
            let (host, port) = parse_pattern(target_str)
                .with_context(|| format!("network.rules.{rule_name}.allow"))?;
            targets.push(InjectTarget {
                host: host.to_string(),
                port,
                secrets: secrets.clone(),
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
            Some((host, after)) => (host, after.strip_prefix(':')),
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

/// Parse a `host[:port]` pattern into its matcher parts, with the port
/// validated. `None` means "any port" and is produced only by an absent
/// port or a literal `*`; every other port string is an error.
///
/// This must never fall back to the wildcard: under deny-by-default, a
/// silently widened `allow = ["*:8O80"]` (letter O) or `["api.example.com:https"]`
/// would grant every port instead of none, and a typo'd inject target would
/// inject the secret into every port on that host.
pub fn parse_pattern(target: &str) -> anyhow::Result<(&str, Option<u16>)> {
    let (host, port) = parse_target(target);
    let port = match port {
        None | Some("*") => None,
        Some(p) => Some(p.parse::<u16>().map_err(|_| {
            anyhow::anyhow!("`{target}`: port `{p}` must be a number in 0-65535 or `*`")
        })?),
    };
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pattern_reads_hostnames_ipv4_and_ipv6_literals() {
        for (pattern, host, port) in [
            ("api.example.com", "api.example.com", None),
            ("api.example.com:443", "api.example.com", Some(443)),
            ("api.example.com:*", "api.example.com", None),
            ("10.0.0.1:80", "10.0.0.1", Some(80)),
            ("*", "*", None),
            ("*:80", "*", Some(80)),
            ("*:*", "*", None),
            ("::1", "::1", None),
            ("2001:db8::1", "2001:db8::1", None),
            ("[::1]", "::1", None),
            ("[::1]:443", "::1", Some(443)),
            ("[2001:db8::1]:8080", "2001:db8::1", Some(8080)),
        ] {
            assert_eq!(parse_pattern(pattern).unwrap(), (host, port), "{pattern}");
        }
    }

    #[test]
    fn parse_pattern_with_malformed_port_fails_instead_of_widening() {
        for bad in [
            "*:8O80",
            "api.example.com:https",
            "api.example.com:443 ",
            "api.example.com:",
            "api.example.com:70000",
            "api.example.com:-1",
            "api.example.com:**",
            "[::1]:x",
            "[::1]:",
        ] {
            let err = parse_pattern(bad).unwrap_err().to_string();
            assert!(err.contains(bad), "{bad}: {err}");
            assert!(err.contains("must be a number"), "{bad}: {err}");
        }
    }
}
