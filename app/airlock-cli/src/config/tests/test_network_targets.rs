use crate::config::config_values;

fn parse(toml_str: &str) -> anyhow::Result<crate::config::config_values::ConfigValues> {
    let value: serde_json::Value = toml::from_str(toml_str).unwrap();
    config_values::parse(value)
}

#[test]
fn well_formed_ports_and_star_are_accepted() {
    let config = parse(
        r#"
        [network.rules.api]
        allow = ["api.example.com", "api.example.com:443", "api.example.com:*", "*:80", "*"]
        deny = ["internal.example.com:*", "[::1]:8080"]

        [network.middleware.mw]
        target = ["api.example.com:*"]
        script = ""
        "#,
    )
    .unwrap();
    assert_eq!(config.network.rules["api"].allow.len(), 5);
}

#[test]
fn malformed_port_in_allow_is_an_error() {
    let err = parse(
        r#"
        [network.rules.api]
        allow = ["*:8O80"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("invalid configuration"), "got: {err}");
    assert!(err.contains("`network.rules.api.allow`"), "got: {err}");
    assert!(err.contains("`*:8O80`"), "got: {err}");
    assert!(err.contains("port `8O80`"), "got: {err}");
}

#[test]
fn malformed_port_in_deny_is_an_error() {
    let err = parse(
        r#"
        [network.rules.api]
        allow = ["api.example.com"]
        deny = ["internal.example.com:https"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`network.rules.api.deny`"), "got: {err}");
    assert!(err.contains("`internal.example.com:https`"), "got: {err}");
}

#[test]
fn malformed_port_in_middleware_target_is_an_error() {
    let err = parse(
        r#"
        [network.middleware.mw]
        target = ["api.example.com:443 "]
        script = ""
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`network.middleware.mw.target`"), "got: {err}");
    assert!(err.contains("`api.example.com:443 `"), "got: {err}");
}

#[test]
fn every_malformed_pattern_is_reported_at_once() {
    let err = parse(
        r#"
        [network.rules.a]
        allow = ["a.example.com:x"]

        [network.rules.b]
        allow = ["b.example.com:y"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`network.rules.a.allow`"), "got: {err}");
    assert!(err.contains("`network.rules.b.allow`"), "got: {err}");
}

#[test]
fn disabled_rule_is_not_validated() {
    // A rule disabled via `enabled = false` (e.g. an inherited pack rule
    // the user cannot edit) is skipped, like the inject checks.
    parse(
        r#"
        [network.rules.api]
        enabled = false
        allow = ["*:8O80"]
        "#,
    )
    .unwrap();
}
