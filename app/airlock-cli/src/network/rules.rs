//! Network rule resolution.
//!
//! Converts the `network` config section into target lists: allow, deny and
//! passthrough targets, middleware targets, inject targets and port forwards.
//! Also compiles the on-demand rules, which apply only to approved commands,
//! and keeps the rules that can change while the sandbox runs. Also parses `host[:port]` target patterns.

use std::collections::BTreeMap;
use std::rc::Rc;

use anyhow::Context;

use super::http;
use super::middleware::LogFn;
use super::target::{InjectTarget, InjectedSecret, MiddlewareTarget, NetworkTarget};
use crate::config::config_values::{Network, NetworkRule, RuleEnabled};
use crate::project::SandboxEnv;
use crate::vault::Vault;

/// Rule targets from the enabled rules.
#[derive(Debug, Default)]
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

/// An on-demand rule, compiled as a normal rule. It applies only to the
/// connections of a command that the user approved.
#[derive(Debug)]
pub struct OnDemandRule {
    /// Allow patterns as in the config.
    pub allow_patterns: Vec<String>,
    /// Allow, deny and passthrough targets.
    pub targets: RuleTargets,
    /// Inject targets of the rule.
    pub inject: Vec<InjectTarget>,
}

/// The network rules of a sandbox, compiled. An approved rule change from
/// the sandbox replaces the rules while the sandbox runs. New connections
/// use the new rules. Open connections and active grants keep the rules
/// that they started with.
#[derive(Debug)]
pub(crate) struct RuleSet {
    /// Network config with the rules as they apply now.
    config: Network,
    /// Masked secrets of each rule with `inject`, or the error text if the
    /// secrets are not available. A disabled rule can have an error. It
    /// shows when the rule is enabled.
    secrets: BTreeMap<String, Result<Vec<InjectedSecret>, String>>,
    /// Allow, deny and passthrough targets of the enabled rules.
    pub active: RuleTargets,
    /// Inject targets of the enabled rules.
    pub inject: Vec<InjectTarget>,
    /// On-demand rules by name.
    pub on_demand: BTreeMap<String, Rc<OnDemandRule>>,
}

impl RuleSet {
    /// Compile the rules of the network config.
    /// Args:
    ///  - `config`: Network config
    ///  - `env`: Sandbox environment with the masked secrets.
    ///
    /// Returns:
    ///   The rules, or error if a used rule has a pattern that is not valid
    ///   or a missing secret.
    pub(crate) fn new(config: &Network, env: &SandboxEnv) -> anyhow::Result<Self> {
        let secrets = config
            .rules
            .iter()
            .filter(|(_, r)| !r.inject.is_empty())
            .map(|(name, rule)| {
                let secrets = rule_secrets(name, rule, env).map_err(|e| format!("{e:#}"));
                (name.clone(), secrets)
            })
            .collect();
        Self::build(config.clone(), secrets)
    }

    /// Compile the used rules of `config`.
    fn build(
        config: Network,
        secrets: BTreeMap<String, Result<Vec<InjectedSecret>, String>>,
    ) -> anyhow::Result<Self> {
        let mut active = RuleTargets::default();
        let mut inject = Vec::new();
        let mut on_demand = BTreeMap::new();
        for (name, rule) in config.rules.iter().filter(|(_, r)| r.is_used()) {
            if rule.passthrough && !rule.inject.is_empty() {
                anyhow::bail!(crate::messages::config::rule_inject_with_passthrough(name));
            }
            let targets = rule_targets(name, &rule.allow, &rule.deny, rule.passthrough)?;
            let rule_secrets = match secrets.get(name) {
                None => Vec::new(),
                Some(Ok(s)) => s.clone(),
                Some(Err(e)) => anyhow::bail!("{e}"),
            };
            let rule_inject = inject_targets(name, &rule.allow, &rule_secrets)?;
            if rule.enabled == RuleEnabled::OnDemand {
                let compiled = OnDemandRule {
                    allow_patterns: rule.allow.clone(),
                    targets,
                    inject: rule_inject,
                };
                on_demand.insert(name.clone(), Rc::new(compiled));
            } else {
                active.allow.extend(targets.allow);
                active.deny.extend(targets.deny);
                active.passthrough.extend(targets.passthrough);
                inject.extend(rule_inject);
            }
        }
        Ok(Self {
            config,
            secrets,
            active,
            inject,
            on_demand,
        })
    }

    /// Get all rules of the config by name, as they apply now.
    pub(crate) fn rules(&self) -> &BTreeMap<String, NetworkRule> {
        &self.config.rules
    }

    /// Add the rule `name`, or replace it. Nothing changes on error.
    /// Returns:
    ///   Error if the rule does not compile, or if its passthrough hosts
    ///   conflict with interception.
    pub(crate) fn upsert(&mut self, name: &str, rule: NetworkRule) -> anyhow::Result<()> {
        *self = self.with_rule(name, rule)?;
        Ok(())
    }

    /// Check that [`RuleSet::upsert`] with these values can succeed.
    pub(crate) fn check(&self, name: &str, rule: NetworkRule) -> anyhow::Result<()> {
        self.with_rule(name, rule).map(drop)
    }

    /// Make a copy of the rules with the rule `name` added or replaced.
    fn with_rule(&self, name: &str, rule: NetworkRule) -> anyhow::Result<Self> {
        let mut config = self.config.clone();
        config.rules.insert(name.to_string(), rule);
        super::check_passthrough(&config)?;
        Self::build(config, self.secrets.clone())
    }
}

/// Convert the patterns of one rule into targets.
/// Config loading reports pattern errors earlier, with the rule name. The
/// errors here are the fail-closed backstop.
/// Returns:
///   The targets, or error if a pattern is not valid.
pub(crate) fn rule_targets(
    rule_name: &str,
    allow: &[String],
    deny: &[String],
    passthrough: bool,
) -> anyhow::Result<RuleTargets> {
    let mut out = RuleTargets::default();
    for target_str in allow {
        let (host, port) = parse_pattern(target_str)
            .with_context(|| crate::messages::network::rule_allow_context(rule_name))?;
        let target = NetworkTarget {
            host: host.to_string(),
            port,
        };
        if passthrough {
            out.passthrough.push(target.clone());
        }
        out.allow.push(target);
    }
    for target_str in deny {
        let (host, port) = parse_pattern(target_str)
            .with_context(|| crate::messages::network::rule_deny_context(rule_name))?;
        out.deny.push(NetworkTarget {
            host: host.to_string(),
            port,
        });
    }
    Ok(out)
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
                .with_context(|| crate::messages::network::middleware_target_context(mw_name))?;
            targets.push(MiddlewareTarget {
                host: host.to_string(),
                port,
                middleware: compiled.clone(),
            });
        }
    }

    Ok(targets)
}

/// Get the masked secrets that one rule injects.
/// Returns:
///   The secrets with no duplicates. Empty if all entries are omitted.
///   Error if a secret is missing.
fn rule_secrets(
    rule_name: &str,
    rule: &NetworkRule,
    env: &SandboxEnv,
) -> anyhow::Result<Vec<InjectedSecret>> {
    let mut secrets: Vec<InjectedSecret> = Vec::with_capacity(rule.inject.len());
    for name in &rule.inject {
        // Config loading makes sure that each name is a masked `[env]`
        // entry and that no injecting rule is `passthrough`.
        // `project::open` and `Project::with_config` make sure that the
        // values are injectable. A missing entry still gives an error,
        // not a panic. An optional entry with no value has nothing to
        // inject.
        if env.is_omitted(name) {
            continue;
        }
        let Some(secret) = env.masked(name) else {
            anyhow::bail!(crate::messages::network::inject_not_masked(rule_name, name));
        };
        if !secrets.iter().any(|s| s.name == *name) {
            secrets.push(InjectedSecret::new(secret.clone()));
        }
    }
    Ok(secrets)
}

/// Make the inject targets of one rule: one for each allow pattern.
/// Returns:
///   The targets. Empty if the rule has no secrets. Error if a pattern is
///   not valid.
fn inject_targets(
    rule_name: &str,
    allow: &[String],
    secrets: &[InjectedSecret],
) -> anyhow::Result<Vec<InjectTarget>> {
    let mut targets = Vec::new();
    // All entries are omitted. Then the rule is a plain allow rule.
    if secrets.is_empty() {
        return Ok(targets);
    }
    for target_str in allow {
        let (host, port) = parse_pattern(target_str)
            .with_context(|| crate::messages::network::rule_allow_context(rule_name))?;
        targets.push(InjectTarget {
            host: host.to_string(),
            port,
            secrets: secrets.to_vec(),
        });
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
        Some(p) => Some(
            p.parse::<u16>()
                .map_err(|_| anyhow::anyhow!(crate::messages::network::invalid_port(target, p)))?,
        ),
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
