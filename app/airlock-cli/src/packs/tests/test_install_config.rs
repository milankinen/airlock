//! Tests of the config of the install boot: an open network and none of
//! the other grants of the project config.

use crate::config::config_values::{ConfigValues, Policy};
use crate::config::{ConfigOverrides, LayeredConfig};
use crate::packs::install::phase::install_config;
use crate::test_cfg::block_on;
use crate::vault::{Vault, VaultStorageType};

/// A user config that grants a bit of everything: env from the host,
/// network rules, middleware, services, ports, sockets, mounts, daemons,
/// masks and clipboard.
const BUSY_USER: &str = r#"
presets = ["claude-code"]

[env]
PLAIN = "1"
FROM_HOST = "${HOME}"
DEFAULTED = "${NOPE:x}"

[network]
policy = "deny-by-default"

[network.rules.mine]
allow = ["example.com"]
deny = ["bad.com"]

[network.rules.pt]
allow = ["pt.example.com"]
passthrough = true

[network.middleware.mw]
target = ["example.com"]
script = "return"

[network.services]
anthropic = true

[network.ports.web]
host = ["8080:80"]

[network.sockets.docker]
host = "/tmp/x.sock:/run/x.sock"

[mounts.data]
source = "/tmp"
target = "/data"

[daemons.d]
command = ["sleep", "1"]

[mask.m]
paths = ["secret"]

[clipboard]
copy = true
paste = true
"#;

/// Resolve a user config (empty: none) and a project config against the
/// built-in packs.
fn resolve(user: &str, project: &str) -> ConfigValues {
    let mut users = vec![];
    if !user.is_empty() {
        users.push(("user", toml::from_str(user).unwrap()));
    }
    let layers = LayeredConfig::from_values(
        users,
        None,
        vec![("airlock.toml", toml::from_str(project).unwrap())],
    )
    .unwrap();
    let packs = crate::packs::init().unwrap();
    let data = crate::test_cfg::temp_dir();
    block_on(layers.resolve(data.path(), &packs, &ConfigOverrides::default()))
        .unwrap()
        .values
}

/// Test that the install boot gets an open network and nothing else that
/// the config grants. The install boot runs vendor installers, so it must
/// not get secrets, mounts or other access.
///   1. Narrow a config that grants a bit of everything
///   2. Check that only the install rule is enabled and that it allows all
///      targets in passthrough mode
///   3. Check that inject, middleware, services, ports, sockets, mounts,
///      daemons, masks and clipboard are all off
///   4. Check that only the env that reads no host variables stays
///   5. Check that the narrowed config passes the network and env checks
#[test]
fn install_config_opens_network_and_grants_nothing_else() {
    let c = install_config(resolve(BUSY_USER, "[packs]\npython = { version = 1 }\n")).unwrap();
    assert_eq!(c.network.policy, Policy::AllowAlways);
    let enabled: Vec<&str> = c
        .network
        .rules
        .iter()
        .filter(|(_, r)| r.enabled)
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(enabled, ["airlock-install"]);
    // The pack rule stays in the config, but it is disabled.
    assert!(c.network.rules.contains_key("python-packages"));
    let rule = &c.network.rules["airlock-install"];
    assert_eq!(rule.allow, ["*"]);
    assert!(rule.passthrough && rule.deny.is_empty());
    assert!(c.network.rules.values().all(|r| r.inject.is_empty()));
    assert!(c.network.middleware.values().all(|m| !m.enabled));
    assert!(c.network.services.is_empty());
    assert!(c.network.ports.values().all(|p| !p.enabled));
    assert!(c.network.sockets.values().all(|s| !s.enabled));
    assert!(c.mounts.values().all(|m| !m.enabled));
    assert!(c.daemons.values().all(|d| !d.enabled));
    assert!(c.mask.values().all(|m| !m.enabled));
    assert!(!c.clipboard.copy && !c.clipboard.paste);
    // The claude-code preset adds this token as a masked secret.
    assert!(!c.env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
    assert!(!c.env.contains_key("FROM_HOST"));
    assert!(!c.env.contains_key("DEFAULTED"));
    assert_eq!(c.env["PLAIN"].value, "1");
    crate::network::check_passthrough(&c.network).unwrap();
    crate::project::resolve_env(&c, &Vault::for_storage_type(VaultStorageType::Disabled)).unwrap();
}

/// Test that the install boot keeps a `HOME` that reads host variables,
/// unless `HOME` is masked. Tools install into `HOME`, and the run boot
/// must find them in the same place.
///   1. Narrow a config whose `HOME` and other env read host variables
///   2. Check that `HOME` stays and the other env is removed
///   3. Narrow a config with a masked `HOME` and check that it is removed
#[test]
fn install_config_keeps_run_home_unless_masked() {
    let c = install_config(resolve(
        "",
        "[env]\nHOME = \"${AIRLOCK_TEST_HOME}/h\"\nOTHER = \"${X}\"\n",
    ))
    .unwrap();
    assert_eq!(c.env["HOME"].value, "${AIRLOCK_TEST_HOME}/h");
    assert!(!c.env.contains_key("OTHER"));
    let c = install_config(resolve(
        "",
        "[env]\nHOME = { value = \"/secret\", mask = true }\n",
    ))
    .unwrap();
    assert!(!c.env.contains_key("HOME"));
}
