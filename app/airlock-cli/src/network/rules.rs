//! Network rule resolution.
//!
//! Converts the `network` config section into target lists: allow, deny and
//! passthrough targets, middleware targets, inject targets and port forwards.
//! Also parses `host[:port]` target patterns.

use anyhow::Context;

use super::http;
use super::middleware::LogFn;
use super::target::{InjectTarget, InjectedSecret, MiddlewareTarget, NetworkTarget};
use crate::config::config_values::Network;
use crate::project::SandboxEnv;
use crate::vault::Vault;

/// Rule targets from the enabled rules.
#[derive(Debug)]
pub struct RuleTargets {
    /// Targets from `allow` lists.
    pub allow: Vec<NetworkTarget>,
    /// Targets from `deny` lists.
    pub deny: Vec<NetworkTarget>,
    /// Subset of `allow`: the targets of rules with `passthrough = true`.
    /// This is a separate list, so the connect path can skip interception
    /// without a new scan of the rule metadata.
    pub passthrough: Vec<NetworkTarget>,
}

/// Convert the enabled config rules into allow, deny and passthrough
/// target lists.
/// Returns:
///   The target lists, or error if a pattern is not valid
///   (see [`parse_pattern`]).
pub fn resolve(network: &Network) -> anyhow::Result<RuleTargets> {
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    let mut passthrough = Vec::new();

    // Config loading reports pattern errors earlier, with the rule name.
    // The errors here are the fail-closed backstop.
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

/// Compile the enabled middleware from the `network.middleware` config
/// section.
/// Args:
///  - `network`: Network config
///  - `vault`: Vault for resolving middleware environment variables
///  - `log`: Logger callback for the in-script `log` function.
///
/// Returns:
///   One middleware target for each target pattern of each middleware, or
///   error if a script does not compile or a pattern is not valid.
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

/// Convert the `inject` lists of the enabled rules into targets with the
/// masked secrets.
/// Args:
///  - `network`: Network config
///  - `env`: Sandbox environment with the masked secrets.
///
/// Returns:
///   One inject target for each allow pattern of each injecting rule, or
///   error if a secret is missing or a pattern is not valid.
pub fn resolve_inject(network: &Network, env: &SandboxEnv) -> anyhow::Result<Vec<InjectTarget>> {
    let mut targets = Vec::new();

    for (rule_name, rule) in &network.rules {
        if !rule.enabled || rule.inject.is_empty() {
            continue;
        }

        let mut secrets: Vec<InjectedSecret> = Vec::with_capacity(rule.inject.len());
        for name in &rule.inject {
            // Config loading makes sure that each name is a masked `[env]`
            // entry and that no injecting rule is `passthrough`.
            // `project::open` and `Project::with_config` make sure that the
            // values are injectable. A missing entry still gives an error,
            // not a panic.
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

/// Get the guest-to-host port forwards from the config.
/// Returns:
///   `(guest_port, host_port)` pairs from all enabled port forward groups.
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

/// Get the host-to-guest port forwards from the config.
/// The host listens on `127.0.0.1:<host_port>`. Each connection goes to
/// `127.0.0.1:<guest_port>` in the guest.
/// Returns:
///   `(host_port, guest_port)` pairs from all enabled port forward groups.
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

/// Split a `host[:port]` target pattern into host and port string.
///
/// Supported forms:
/// - `[::1]` and `[::1]:443`: IPv6 in brackets. The port follows `]`.
/// - `2001:db8::1`: IPv6 with no brackets (more than one colon). This form
///   has no `:port`, so the full string is the host.
/// - All other forms: a hostname or IPv4 with an optional `:port`.
///
/// Returns:
///   `(host, port)`. The port is `None` if the pattern has no port.
pub(super) fn parse_target(target: &str) -> (&str, Option<&str>) {
    // A simple `rsplit_once(':')` breaks IPv6 literals. For example `::1`
    // would give host `":"` and port `"1"`, which matches nothing.
    if let Some(rest) = target.strip_prefix('[') {
        // IPv6 literal in brackets.
        return match rest.split_once(']') {
            Some((host, after)) => (host, after.strip_prefix(':')),
            None => (target, None), // Not valid: use the full string as host.
        };
    }
    if target.matches(':').count() > 1 {
        // IPv6 literal with no brackets has no port.
        return (target, None);
    }
    match target.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (target, None),
    }
}

/// Parse a `host[:port]` pattern into host and validated port.
/// Returns:
///   `(host, port)`. The port is `None` ("any port") only if the pattern has
///   no port or the port is `*`. All other port strings that are not valid
///   numbers give an error.
pub fn parse_pattern(target: &str) -> anyhow::Result<(&str, Option<u16>)> {
    let (host, port) = parse_target(target);
    // Never use the wildcard as a fallback. With a fallback, a typo such as
    // `allow = ["*:8O80"]` (letter O) or `["api.example.com:https"]` would
    // allow all ports instead of none. An inject target with a typo would
    // inject the secret into all ports of that host.
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
    //! Tests for the parser of `host:port` target patterns.

    use super::*;

    /// Test that the pattern parser reads host names, IPv4 and IPv6
    /// literals, with and without a port. A bare IPv6 address has colons, so
    /// it must not be read as a host and a port.
    ///   1. Parse patterns with names, wildcards, IPv4 and IPv6 literals
    ///   2. Check the host and the port (none for no port or `*`)
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

    /// Test that a pattern with a bad port fails. If the parser ignored the
    /// port, the rule would match all ports.
    ///   1. Parse patterns with ports that are not valid numbers
    ///   2. Check that each fails with an error that names the pattern
    #[test]
    fn parse_pattern_with_malformed_port_fails_instead_of_widening() {
        for bad in [
            // The letter O, not the digit 0.
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
