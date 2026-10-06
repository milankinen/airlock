//! `[network.services]`: names, layering, the packs that enable them,
//! and their conflict with passthrough rules.

use crate::config::LayeredConfig;
use crate::config::config_values::{self, ConfigValues};
use crate::test_support::{block_on, resolve_project_toml};

fn parse(toml_str: &str) -> anyhow::Result<ConfigValues> {
    let value: serde_json::Value = toml::from_str(toml_str).unwrap();
    config_values::parse(value)
}

#[test]
fn services_are_off_by_default_and_parse_by_name() {
    assert!(parse("").unwrap().network.services.is_empty());
    let config = parse("[network.services]\nanthropic = true\nopenai = false\n").unwrap();
    let enabled = crate::services::enabled(&config.network.services);
    assert_eq!(enabled, [crate::services::ServiceId::Anthropic]);
}

#[test]
fn an_unknown_service_is_an_error_naming_the_known_ones() {
    let err = format!(
        "{:#}",
        parse("[network.services]\nanthropic = true\ngemini = true\n").unwrap_err()
    );
    assert!(err.contains("invalid configuration"), "{err}");
    assert!(
        err.contains("* `network.services.gemini` unknown service (known: anthropic, openai)"),
        "{err}"
    );
}

#[test]
fn a_service_value_must_be_a_bool() {
    assert!(parse("[network.services]\nanthropic = \"yes\"\n").is_err());
}

/// Later layers override single services; the others stay.
#[test]
fn services_merge_across_layers() {
    let layers = LayeredConfig::from_values(
        vec![(
            "~/.airlock/config.toml",
            toml::from_str("[network.services]\nanthropic = true\nopenai = true\n").unwrap(),
        )],
        None,
        vec![(
            "airlock.toml",
            toml::from_str("[network.services]\nopenai = false\n").unwrap(),
        )],
    )
    .unwrap();
    let resolved = block_on(layers.resolve(
        &crate::packs::init_with_sample(),
        &crate::config::ConfigOverrides::default(),
    ))
    .unwrap();
    let services = &resolved.values.network.services;
    assert_eq!(services.get("anthropic"), Some(&true));
    assert_eq!(services.get("openai"), Some(&false));
}

/// The agent packs enable their service and leave its hosts to it.
#[test]
fn the_agent_packs_enable_their_service() {
    let claude = resolve_project_toml("[packs]\nclaude = { version = 1 }\n").unwrap();
    let net = &claude.values.network;
    assert_eq!(net.services.get("anthropic"), Some(&true));
    let rule = &net.rules["claude-code"];
    assert_eq!(rule.allow, ["claude.ai:443", "downloads.claude.ai:443"]);
    crate::network::check_passthrough(net).unwrap();

    let codex = resolve_project_toml("[packs]\ncodex = { version = 1 }\n").unwrap();
    let net = &codex.values.network;
    assert_eq!(net.services.get("openai"), Some(&true));
    assert_eq!(net.rules["codex"].allow, ["api.openai.com:443"]);
    crate::network::check_passthrough(net).unwrap();

    // A project can switch a pack's service off.
    let off = resolve_project_toml(
        "[packs]\nclaude = { version = 1 }\n[network.services]\nanthropic = false\n",
    )
    .unwrap();
    assert!(crate::services::enabled(&off.values.network.services).is_empty());
}

/// A service needs its hosts intercepted: a passthrough rule over one of
/// them is a config error, unless the service is off.
#[test]
fn a_passthrough_rule_on_a_service_host_is_an_error() {
    let toml = |on: bool| {
        format!(
            "[network.services]\nanthropic = {on}\n\
             [network.rules.raw]\nallow = [\"*.anthropic.com\"]\npassthrough = true\n"
        )
    };
    let err = format!(
        "{:#}",
        crate::network::check_passthrough(&parse(&toml(true)).unwrap().network).unwrap_err()
    );
    assert!(
        err.contains("rule `raw` allow=`*.anthropic.com` (passthrough) conflicts with service `anthropic` host `api.anthropic.com:443`"),
        "{err}"
    );
    crate::network::check_passthrough(&parse(&toml(false)).unwrap().network).unwrap();
}
