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
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::config_values::{self, NetworkRule, Policy};
    use crate::project::MaskedSecret;

    fn rule(allow: &[&str], passthrough: bool) -> NetworkRule {
        NetworkRule {
            enabled: true,
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: vec![],
            passthrough,
            inject: vec![],
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
        assert_eq!(parse_target("[::1]:"), ("::1", Some("")));
        assert_eq!(parse_target("[::1]:443"), ("::1", Some("443")));
        assert_eq!(
            parse_target("[2001:db8::1]:8080"),
            ("2001:db8::1", Some("8080"))
        );
    }

    #[test]
    fn parse_pattern_accepts_numeric_and_star_ports() {
        assert_eq!(
            parse_pattern("api.example.com").unwrap(),
            ("api.example.com", None)
        );
        assert_eq!(
            parse_pattern("api.example.com:443").unwrap(),
            ("api.example.com", Some(443))
        );
        assert_eq!(
            parse_pattern("api.example.com:*").unwrap(),
            ("api.example.com", None)
        );
        assert_eq!(parse_pattern("*:80").unwrap(), ("*", Some(80)));
        assert_eq!(parse_pattern("*:*").unwrap(), ("*", None));
        assert_eq!(parse_pattern("*").unwrap(), ("*", None));
        assert_eq!(parse_pattern("[::1]:443").unwrap(), ("::1", Some(443)));
        assert_eq!(parse_pattern("2001:db8::1").unwrap(), ("2001:db8::1", None));
    }

    #[test]
    fn parse_pattern_rejects_malformed_ports() {
        // Every one of these used to resolve to "any port".
        for bad in [
            "*:8O80", // letter O
            "api.example.com:https",
            "api.example.com:443 ", // trailing space
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

    #[test]
    fn resolve_rejects_malformed_port_instead_of_widening() {
        let mut rules = BTreeMap::new();
        rules.insert("api".to_string(), rule(&["*:8O80"], false));
        let err = resolve(&net_with_rules(rules)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("network.rules.api.allow"), "got: {msg}");
        assert!(msg.contains("`*:8O80`"), "got: {msg}");

        let mut rules = BTreeMap::new();
        let mut r = rule(&["api.example.com"], false);
        r.deny = vec!["internal.example.com:https".to_string()];
        rules.insert("api".to_string(), r);
        let msg = format!("{:#}", resolve(&net_with_rules(rules)).unwrap_err());
        assert!(msg.contains("network.rules.api.deny"), "got: {msg}");
    }

    #[test]
    fn resolve_treats_star_port_as_any_port() {
        let mut rules = BTreeMap::new();
        rules.insert("api".to_string(), rule(&["api.example.com:*"], false));
        let resolved = resolve(&net_with_rules(rules)).unwrap();
        assert!(resolved.allow[0].matches("api.example.com", 443));
        assert!(resolved.allow[0].matches("api.example.com", 8080));
    }

    #[test]
    fn resolve_inject_rejects_malformed_port() {
        let env = SandboxEnv::from_secrets(vec![secret("A", "aaaaaaaaaaaa")]);
        let net = net_with(vec![(
            "api",
            inject_rule(&["api.example.com:443 "], &["A"], false),
        )]);
        let msg = format!("{:#}", resolve_inject(&net, &env).unwrap_err());
        assert!(msg.contains("network.rules.api.allow"), "got: {msg}");
        assert!(msg.contains("`api.example.com:443 `"), "got: {msg}");
    }

    fn net_with_rules(rules: BTreeMap<String, NetworkRule>) -> config_values::Network {
        config_values::Network {
            policy: Policy::DenyByDefault,
            rules,
            middleware: BTreeMap::default(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
            services: BTreeMap::default(),
        }
    }

    #[test]
    fn passthrough_rule_contributes_to_both_allow_and_passthrough() {
        let mut rules = BTreeMap::new();
        rules.insert("pt".to_string(), rule(&["db.example.com:5432"], true));
        rules.insert("plain".to_string(), rule(&["api.example.com"], false));
        let net = config_values::Network {
            policy: Policy::DenyByDefault,
            rules,
            middleware: BTreeMap::default(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
            services: BTreeMap::default(),
        };
        let resolved = resolve(&net).unwrap();
        assert_eq!(resolved.allow.len(), 2);
        assert_eq!(resolved.passthrough.len(), 1);
        assert!(resolved.passthrough[0].matches("db.example.com", 5432));
        assert!(!resolved.passthrough[0].matches("api.example.com", 443));
    }

    fn secret(name: &str, real: &str) -> MaskedSecret {
        MaskedSecret {
            name: name.to_string(),
            real: real.to_string(),
            surrogate: "x".repeat(real.chars().count()),
        }
    }

    fn net_with(rules: Vec<(&str, NetworkRule)>) -> config_values::Network {
        config_values::Network {
            policy: Policy::DenyByDefault,
            rules: rules.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            middleware: BTreeMap::default(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
            services: BTreeMap::default(),
        }
    }

    fn inject_rule(allow: &[&str], inject: &[&str], passthrough: bool) -> NetworkRule {
        NetworkRule {
            enabled: true,
            allow: allow.iter().map(|s| (*s).to_string()).collect(),
            deny: vec![],
            passthrough,
            inject: inject.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn resolve_inject_builds_one_target_per_allow_pattern() {
        let env = SandboxEnv::from_secrets(vec![
            secret("A", "aaaaaaaaaaaa"),
            secret("B", "bbbbbbbbbbbb"),
        ]);
        let net = net_with(vec![
            (
                "api",
                inject_rule(
                    &["api.example.com:443", "*.example.org"],
                    &["A", "B", "A"],
                    false,
                ),
            ),
            ("plain", inject_rule(&["plain.example.com"], &[], false)),
        ]);
        let targets = resolve_inject(&net, &env).unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets[0].matches("api.example.com", 443));
        assert!(!targets[0].matches("api.example.com", 80));
        assert!(targets[1].matches("x.example.org", 1234));
        // Duplicate names collapse; both patterns share the same secrets.
        assert_eq!(targets[0].secrets.len(), 2);
        assert!(InjectedSecret::ptr_eq(
            &targets[0].secrets[0],
            &targets[1].secrets[0]
        ));
    }

    #[test]
    fn resolve_inject_skips_disabled_rules() {
        let env = SandboxEnv::from_secrets(vec![secret("A", "aaaaaaaaaaaa")]);
        let mut rule = inject_rule(&["api.example.com"], &["A"], false);
        rule.enabled = false;
        let net = net_with(vec![("api", rule)]);
        assert!(resolve_inject(&net, &env).unwrap().is_empty());
    }

    #[test]
    fn resolve_inject_rejects_unknown_name() {
        let env = SandboxEnv::from_secrets(vec![]);
        let net = net_with(vec![(
            "api",
            inject_rule(&["api.example.com"], &["A"], false),
        )]);
        let err = resolve_inject(&net, &env).unwrap_err().to_string();
        assert!(err.contains("network.rules.api.inject"), "got: {err}");
        assert!(err.contains("`A`"), "got: {err}");
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
                inject: vec![],
            },
        );
        let net = config_values::Network {
            policy: Policy::DenyByDefault,
            rules,
            middleware: BTreeMap::default(),
            ports: BTreeMap::default(),
            sockets: BTreeMap::default(),
            services: BTreeMap::default(),
        };
        let resolved = resolve(&net).unwrap();
        assert!(resolved.allow.is_empty());
        assert!(resolved.passthrough.is_empty());
    }
}
