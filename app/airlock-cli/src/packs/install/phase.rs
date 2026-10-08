//! Config and boot options of the setup boots.
//!
//! Narrows the project config for the install boot. The install boot gets
//! open access to the public internet, but no secrets, mounts or other
//! grants of the config.

use crate::cli::LogLevel;
use crate::config::config_values::{self, ConfigValues, NetworkRule, Policy};
use crate::sandbox::boot::BootOptions;

/// Name of the install boot network rule. The rule allows all targets
/// in passthrough mode.
const INSTALL_RULE: &str = "airlock-install";

/// Narrow the resolved config for the install boot and validate it.
///
/// The install boot runs code that the user did not write (vendor
/// installers) and code that is already on the persistent disk. It gets
/// open access to the public internet without TLS interception. The host
/// loopback, the LAN and cloud metadata stay out of reach.
///
/// The install boot gets nothing else that the config grants: no secrets
/// (no masked env, no `${…}` env except `HOME`, no inject), no mounts, no
/// project share, and no ports, sockets, daemons, masks, middleware or
/// clipboard.
/// Args:
///  - `config`: The resolved config
///
/// Returns:
///   The narrowed config, or an error if it is not valid.
pub fn install_config(mut config: ConfigValues) -> anyhow::Result<ConfigValues> {
    // Open network: allow all, disable the user rules and add one
    // passthrough rule for all targets. The install boot uses
    // `Network::public_only`, thus the rule reaches only the public internet.
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

/// Get the boot options of the setup boots. The boots are quiet because
/// they print their own progress. They have no project share.
pub fn boot_options(log_level: LogLevel) -> BootOptions {
    BootOptions {
        log_level,
        quiet: true,
        project_share: false,
    }
}

/// Remove what the setup boots never get: middleware, network services,
/// ports, sockets, secrets and host env (except a `HOME` that is not
/// masked), mounts, daemons, masks and clipboard.
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
    // Keep `HOME` even if it reads host variables. Tools install into it
    // (`~/.cargo`, `~/.local/bin`), and the run boot must find them in the
    // same place. The run config resolves `HOME` the same way.
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

/// Check if the `[env]` template `value` reads a variable (`$X`, `${X}`,
/// `${X:default}`).
fn references_vars(value: &str) -> bool {
    // Use the substitution parser itself to find variable reads. A
    // template that it cannot parse counts as one that reads a variable.
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
    //! Tests of the env narrowing of the setup boots.

    use super::*;

    /// Test that the variable check follows the substitution syntax. Env
    /// that reads host variables must not reach the install boot.
    ///   1. Check that `$X`, `${X}`, `${X:default}` and a template that does
    ///      not parse count as variable reads
    ///   2. Check that plain text, an escaped `$` and empty text do not
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
