//! The setup boots' configs: the resolved config, narrowed.
//!
//! The install boot runs code the user did not write (vendor installers)
//! and code already on the persistent disk, with an open network: the
//! policy is `allow-always`, every rule is disabled, and one
//! [`INSTALL_RULE`] passes every target through without TLS interception.
//! The open network is the public internet only: the install boot's
//! network is [`crate::network::Network::public_only`], so the host's
//! loopback, the LAN and cloud metadata stay out of reach.
//! It gets nothing else the config grants: no secrets (no masked or
//! `${…}` env, no inject), no mounts and no project share, and no ports,
//! sockets, daemons, masks, middleware or clipboard.

use crate::cli::LogLevel;
use crate::config::config_values::{self, ConfigValues, NetworkRule, Policy};
use crate::sandbox::boot::BootOptions;

/// The network rule of the install boot: every target, passthrough.
const INSTALL_RULE: &str = "airlock-install";

/// Narrow the resolved `config` for the install boot (see the module
/// docs) and validate the result.
pub fn install_config(mut config: ConfigValues) -> anyhow::Result<ConfigValues> {
    let net = &mut config.network;
    net.policy = Policy::AllowAlways;
    for rule in net.rules.values_mut() {
        rule.enabled = false;
        rule.inject.clear();
    }
    net.rules.insert(
        INSTALL_RULE.to_string(),
        NetworkRule {
            enabled: true,
            allow: vec!["*".into()],
            deny: vec![],
            passthrough: true,
            inject: vec![],
        },
    );
    isolate(&mut config);
    config_values::validate(&config)?;
    Ok(config)
}

/// The boot options of the setup boots: quiet (they print their own
/// progress) and without the project share.
pub fn boot_options(log_level: LogLevel) -> BootOptions {
    BootOptions {
        log_level,
        quiet: true,
        project_share: false,
    }
}

/// What the setup boots never get: middleware, network services, ports,
/// sockets, secrets and host env (except a `HOME` that is not masked),
/// mounts, daemons, masks, clipboard.
fn isolate(config: &mut ConfigValues) {
    let net = &mut config.network;
    net.services.clear();
    for mw in net.middleware.values_mut() {
        mw.enabled = false;
    }
    for port in net.ports.values_mut() {
        port.enabled = false;
    }
    for socket in net.sockets.values_mut() {
        socket.enabled = false;
    }
    // `HOME` stays even when it reads host variables: tools install into
    // it (`~/.cargo`, `~/.local/bin`), and the run boot must find them in
    // the same place. The run config resolves it the same way.
    config
        .env
        .retain(|name, var| !var.mask && (name == "HOME" || !references_vars(&var.value)));
    for mount in config.mounts.values_mut() {
        mount.enabled = false;
    }
    for daemon in config.daemons.values_mut() {
        daemon.enabled = false;
    }
    for mask in config.mask.values_mut() {
        mask.enabled = false;
    }
    config.clipboard.copy = false;
    config.clipboard.paste = false;
}

/// Whether the `[env]` template `value` reads a variable (`$X`, `${X}`,
/// `${X:default}`). Uses the substitution parser itself; a template it
/// cannot parse counts as reading one.
fn references_vars(value: &str) -> bool {
    struct Recorder(std::cell::Cell<bool>);
    impl<'a> subst::VariableMap<'a> for Recorder {
        type Value = &'static str;
        fn get(&'a self, _key: &str) -> Option<Self::Value> {
            self.0.set(true);
            Some("")
        }
    }
    let recorder = Recorder(std::cell::Cell::new(false));
    match subst::substitute(value, &recorder) {
        Ok(_) => recorder.0.get(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::LayeredConfig;
    use crate::vault::{DisabledStorage, Vault, VaultStorageType};

    /// The resolved config values of `layers`.
    fn resolve(layers: &LayeredConfig) -> ConfigValues {
        let packs = crate::packs::init().unwrap();
        crate::test_support::block_on(
            layers.resolve(&packs, &crate::config::ConfigOverrides::default()),
        )
        .unwrap()
        .values
    }

    /// A config that grants everything a config can grant.
    fn busy() -> LayeredConfig {
        busy_with(serde_json::json!({"packs": {"python": {"version": "1"}}}))
    }

    /// [`busy`] with the project file `project`.
    fn busy_with(project: serde_json::Value) -> LayeredConfig {
        let user = serde_json::json!({
                "presets": ["claude-code"],
                "env": {"PLAIN": "1", "FROM_HOST": "${HOME}", "DEFAULTED": "${NOPE:x}"},
                "network": {
                    "policy": "deny-by-default",
                    "rules": {
                        "mine": {"allow": ["example.com"], "deny": ["bad.com"]},
                        "pt": {"allow": ["pt.example.com"], "passthrough": true}
                    },
                    "middleware": {"mw": {"target": ["example.com"], "script": "return"}},
                    "services": {"anthropic": true},
                    "ports": {"web": {"host": ["8080:80"]}},
                    "sockets": {"docker": {"host": "/tmp/x.sock:/run/x.sock"}}
                },
                "mounts": {"data": {"source": "/tmp", "target": "/data"}},
                "daemons": {"d": {"command": ["sleep", "1"]}},
                "mask": {"m": {"paths": ["secret"]}},
                "clipboard": {"copy": true, "paste": true}
        });
        LayeredConfig::from_values(vec![("user", user)], None, vec![("airlock.toml", project)])
            .unwrap()
    }

    #[test]
    fn install_config_opens_the_network_and_grants_nothing() {
        let c = install_config(resolve(&busy())).unwrap();
        assert_eq!(c.network.policy, Policy::AllowAlways);
        // Every rule is off (also the pack's), except the install rule.
        let enabled: Vec<&str> = c
            .network
            .rules
            .iter()
            .filter(|(_, r)| r.enabled)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(enabled, [INSTALL_RULE]);
        assert!(c.network.rules.contains_key("python-packages"));
        let rule = &c.network.rules[INSTALL_RULE];
        assert_eq!(rule.allow, ["*"]);
        assert!(rule.passthrough && rule.deny.is_empty() && rule.inject.is_empty());
        assert!(c.network.rules.values().all(|r| r.inject.is_empty()));
        assert!(c.network.middleware.values().all(|m| !m.enabled));
        assert!(c.network.services.is_empty());
        assert!(c.network.ports.values().all(|p| !p.enabled));
        assert!(c.network.sockets.values().all(|s| !s.enabled));
        assert!(c.mounts.values().all(|m| !m.enabled));
        assert!(c.daemons.values().all(|d| !d.enabled));
        assert!(c.mask.values().all(|m| !m.enabled));
        assert!(!c.clipboard.copy && !c.clipboard.paste);
        // Masked and `${…}` env is gone; literals stay.
        assert!(!c.env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!c.env.contains_key("FROM_HOST"));
        assert!(!c.env.contains_key("DEFAULTED"));
        assert_eq!(c.env["PLAIN"].value, "1");
        // Passthrough-all conflicts with nothing: no middleware, no inject,
        // no service.
        crate::network::check_passthrough(&c.network).unwrap();
        let opts = boot_options(LogLevel::Info);
        assert!(!opts.project_share && opts.quiet);
    }

    /// The install config resolves its env without any secret.
    #[test]
    fn install_config_needs_no_secrets() {
        let c = install_config(resolve(&busy())).unwrap();
        let vault = Vault::new_with(
            Box::new(DisabledStorage),
            HashMap::new(),
            VaultStorageType::Disabled,
        );
        crate::project::resolve_env(&c, &vault).unwrap();
    }

    /// The install boot's `HOME` is the run boot's: a `[env] HOME` that
    /// reads host variables stays (masked, it does not).
    #[test]
    fn install_config_keeps_the_run_home() {
        let layers = |home: serde_json::Value| {
            LayeredConfig::from_values(
                vec![],
                None,
                vec![(
                    "airlock.toml",
                    serde_json::json!({"env": {"HOME": home, "OTHER": "${X}"}}),
                )],
            )
            .unwrap()
        };
        let c = install_config(resolve(&layers(serde_json::json!(
            "${AIRLOCK_TEST_HOME}/h"
        ))))
        .unwrap();
        assert_eq!(c.env["HOME"].value, "${AIRLOCK_TEST_HOME}/h");
        assert!(!c.env.contains_key("OTHER"));
        let masked = serde_json::json!({"value": "/secret", "mask": true});
        let c = install_config(resolve(&layers(masked))).unwrap();
        assert!(!c.env.contains_key("HOME"));
    }

    #[test]
    fn references_vars_uses_the_substitution_syntax() {
        assert!(references_vars("${X}"));
        assert!(references_vars("$X/bin"));
        assert!(references_vars("${X:default}"));
        assert!(references_vars("bad \\q escape"));
        assert!(!references_vars("plain"));
        assert!(!references_vars("/work/\\$X"));
        assert!(!references_vars(""));
    }
}
