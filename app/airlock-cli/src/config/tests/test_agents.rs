use crate::config::config_values::{self, ConfigValues};

fn parse(toml_str: &str) -> anyhow::Result<ConfigValues> {
    let value: serde_json::Value = toml::from_str(toml_str).unwrap();
    config_values::parse(value)
}

/// The unreleased `[agents]` table (`auto_signin`) is gone: a leftover
/// table is ignored, and it is not serialized.
#[test]
fn a_leftover_agents_table_is_ignored() {
    let config = parse("[agents]\nauto_signin = false\n").unwrap();
    let json = serde_json::to_value(config).unwrap();
    assert!(json.get("agents").is_none());
}
