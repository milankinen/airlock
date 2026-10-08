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
    use super::*;

    #[test]
    fn references_vars_follows_substitution_syntax() {
        assert!(references_vars("${X}"));
        assert!(references_vars("$X/bin"));
        assert!(references_vars("${X:default}"));
        assert!(references_vars("bad \\q escape"));
        assert!(!references_vars("plain"));
        assert!(!references_vars("/work/\\$X"));
        assert!(!references_vars(""));
    }
}
