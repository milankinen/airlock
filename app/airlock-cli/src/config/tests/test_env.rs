use crate::config::config_values::{self, EnvVar};
use crate::config::merge::{merge_json, normalize_env};

fn parse(toml_str: &str) -> anyhow::Result<crate::config::config_values::ConfigValues> {
    let value: serde_json::Value = toml::from_str(toml_str).unwrap();
    config_values::parse(value)
}

#[test]
fn plain_string_is_unmasked() {
    let config = parse(
        r#"
        [env]
        EDITOR = "vim"
        "#,
    )
    .unwrap();
    assert_eq!(config.env["EDITOR"], EnvVar::plain("vim"));
}

#[test]
fn table_form_with_mask() {
    let config = parse(
        r#"
        [env]
        TOKEN = { value = "${TOKEN}", mask = true }
        "#,
    )
    .unwrap();
    let var = &config.env["TOKEN"];
    assert_eq!(var.value, "${TOKEN}");
    assert!(var.mask);
}

#[test]
fn table_form_defaults_mask_to_false() {
    let config = parse(
        r#"
        [env]
        TOKEN = { value = "x" }
        "#,
    )
    .unwrap();
    assert_eq!(config.env["TOKEN"], EnvVar::plain("x"));
}

#[test]
fn table_form_without_value_is_an_error() {
    let err = parse(
        r"
        [env]
        TOKEN = { mask = true }
        ",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("TOKEN"), "got: {err}");
}

#[test]
fn table_form_rejects_unknown_keys() {
    // `masked` instead of `mask` must not silently parse as unmasked —
    // that would leak the real value into the guest. And the error must
    // name the offending key so the typo is findable.
    let err = parse(
        r#"
        [env]
        TOKEN = { value = "x", masked = true }
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("TOKEN"), "got: {err}");
    assert!(err.contains("`masked`"), "got: {err}");
}

#[test]
fn table_form_missing_value_names_the_field() {
    let err = parse(
        r"
        [env]
        TOKEN = { mask = true }
        ",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`value`"), "got: {err}");
}

#[test]
fn string_overlay_keeps_base_mask() {
    // A plain string in a later layer (e.g. airlock.local.toml) must only
    // replace the value — never silently un-mask the base layer's entry.
    let mut base: serde_json::Value = toml::from_str(
        r#"
        [env]
        TOKEN = { value = "${A}", mask = true }
        "#,
    )
    .unwrap();
    let mut overlay: serde_json::Value = toml::from_str(
        r#"
        [env]
        TOKEN = "${B}"
        "#,
    )
    .unwrap();
    normalize_env(&mut base);
    normalize_env(&mut overlay);
    let config = config_values::parse(merge_json(base, overlay)).unwrap();
    let var = &config.env["TOKEN"];
    assert_eq!(var.value, "${B}");
    assert!(var.mask, "overlay string must not un-mask the entry");
}

#[test]
fn mask_only_overlay_on_string_base_keeps_value() {
    let mut base: serde_json::Value = toml::from_str(
        r#"
        [env]
        TOKEN = "${A}"
        "#,
    )
    .unwrap();
    let mut overlay: serde_json::Value = toml::from_str(
        r"
        [env]
        TOKEN = { mask = true }
        ",
    )
    .unwrap();
    normalize_env(&mut base);
    normalize_env(&mut overlay);
    let config = config_values::parse(merge_json(base, overlay)).unwrap();
    let var = &config.env["TOKEN"];
    assert_eq!(var.value, "${A}");
    assert!(var.mask);
}

#[test]
fn inject_with_passthrough_is_an_error() {
    let err = parse(
        r#"
        [env]
        TOKEN = { value = "x", mask = true }

        [network.rules.db]
        allow = ["db.example.com:5432"]
        passthrough = true
        inject = ["TOKEN"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("network.rules.db"), "got: {err}");
    assert!(err.contains("passthrough"), "got: {err}");
}

#[test]
fn inject_parses_on_rule() {
    let config = parse(
        r#"
        [env]
        TOKEN = { value = "x", mask = true }

        [network.rules.api]
        allow = ["api.example.com:443"]
        inject = ["TOKEN"]
        "#,
    )
    .unwrap();
    assert_eq!(
        config.network.rules["api"].inject,
        vec!["TOKEN".to_string()]
    );
}

#[test]
fn inject_defaults_to_empty() {
    let config = parse(
        r#"
        [network.rules.api]
        allow = ["api.example.com:443"]
        "#,
    )
    .unwrap();
    assert!(config.network.rules["api"].inject.is_empty());
}

#[test]
fn inject_of_undefined_variable_is_an_error() {
    let err = parse(
        r#"
        [network.rules.api]
        allow = ["api.example.com:443"]
        inject = ["NOPE"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("network.rules.api.inject"), "got: {err}");
    assert!(err.contains("`NOPE`"), "got: {err}");
    assert!(err.contains("mask = true"), "got: {err}");
}

#[test]
fn inject_of_unmasked_variable_is_an_error() {
    let err = parse(
        r#"
        [env]
        TOKEN = "plain"

        [network.rules.api]
        allow = ["api.example.com:443"]
        inject = ["TOKEN"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`TOKEN`"), "got: {err}");
}

#[test]
fn inject_reports_every_offending_name() {
    let err = parse(
        r#"
        [env]
        A = "plain"

        [network.rules.api]
        allow = ["api.example.com:443"]
        inject = ["A", "B"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`A`"), "got: {err}");
    assert!(err.contains("`B`"), "got: {err}");
}

#[test]
fn inject_on_disabled_rule_is_not_validated() {
    parse(
        r#"
        [network.rules.api]
        enabled = false
        allow = ["api.example.com:443"]
        inject = ["NOPE"]
        "#,
    )
    .unwrap();
}
