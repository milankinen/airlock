use std::path::Path;

use smart_config::DescribeConfig;

use crate::config::Config;
use crate::config::de::format_error;
use crate::error::CliError;

/// Load configuration from hierarchical TOML files.
///
/// Files are loaded in order (later overrides former):
/// 1. `~/.ezpez/config.toml`
/// 2. `~/.ez.toml`
/// 3. `<project_root>/ez.toml`
/// 4. `<project_root>/ez.local.toml`
pub fn load(project_root: &Path) -> Result<Config, CliError> {
    let home = dirs::home_dir().unwrap_or_default();
    let paths = [
        home.join(".ezpez/config.toml"),
        home.join(".ez.toml"),
        project_root.join("ez.toml"),
        project_root.join("ez.local.toml"),
    ];

    let mut merged = serde_json::Value::Object(serde_json::Map::new());
    for path in &paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            let value: serde_json::Value = toml::from_str(&content)
                .map_err(|e| CliError::expected(format!("{}: {e}", path.display())))?;
            merged = merge_json(merged, value);
        }
    }

    let serde_json::Value::Object(map) = merged else {
        return Err(CliError::expected("config must be a TOML table"));
    };

    let schema = smart_config::ConfigSchema::new(&Config::DESCRIPTION, "");
    let source = smart_config::Json::new("merged config", map);
    let repo = smart_config::ConfigRepository::new(&schema).with(source);
    let parser = repo.single::<Config>()?;
    match parser.parse() {
        Ok(config) => Ok(config),
        Err(errors) => Err(CliError::expected(format_error(
            "invalid configuration",
            errors,
        ))),
    }
}

/// Merge two JSON values with custom rules:
/// - Arrays: concatenate
/// - Objects: recursive merge
/// - Primitives: overlay wins
/// - Type mismatch: overlay wins
fn merge_json(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (base, overlay) {
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, overlay_val) in overlay {
                let merged = match base.remove(&key) {
                    Some(base_val) => merge_json(base_val, overlay_val),
                    None => overlay_val,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (Value::Array(mut base), Value::Array(overlay)) => {
            base.extend(overlay);
            Value::Array(base)
        }
        (_, overlay) => overlay,
    }
}
