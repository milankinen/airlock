use std::collections::HashSet;
use std::path::Path;

use smart_config::DescribeConfig;

use crate::config::de::format_error;
use crate::config::{Config, presets};

/// Load configuration from hierarchical TOML files.
///
/// Files are loaded in order (later overrides former):
/// 1. `~/.ezpez/config.toml`
/// 2. `~/.ez.toml`
/// 3. `<project_root>/ez.toml`
/// 4. `<project_root>/ez.local.toml`
///
/// If the merged config contains a `presets` array, the named
/// presets are applied as base layers before the user config.
pub fn load(project_root: &Path) -> anyhow::Result<Config> {
    let home = dirs::home_dir().unwrap_or_default();
    let paths = [
        home.join(".ezpez/config.toml"),
        home.join(".ez.toml"),
        project_root.join("ez.toml"),
        project_root.join("ez.local.toml"),
    ];

    // 1. Create base config
    let base = serde_json::Value::Object(serde_json::Map::new());

    // 2. Load user config
    let mut user_config = serde_json::Value::Object(serde_json::Map::new());
    for path in &paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            let value: serde_json::Value =
                toml::from_str(&content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            user_config = merge_json(user_config, value);
        }
    }

    // 3-5. Resolve presets and apply user config on top
    let merged = apply_with_presets(
        base,
        user_config,
        &presets::get,
        &mut vec![],
        &mut HashSet::new(),
    )?;

    parse_config(merged)
}

/// Apply a config layer with preset resolution:
/// 1. Extract presets from the config
/// 2. Recursively apply each preset onto base
/// 3. Apply the config (without presets key) on top
pub(super) fn apply_with_presets(
    mut base: serde_json::Value,
    mut config: serde_json::Value,
    resolve: &dyn Fn(&str) -> Option<serde_json::Value>,
    chain: &mut Vec<String>,
    applied: &mut HashSet<String>,
) -> anyhow::Result<serde_json::Value> {
    let preset_names = extract_presets(&mut config);

    for name in preset_names {
        if applied.contains(&name) {
            continue;
        }
        if chain.contains(&name) {
            anyhow::bail!(
                "circular preset dependency: {} -> {name}",
                chain.join(" -> ")
            );
        }

        let preset_config =
            resolve(&name).ok_or_else(|| anyhow::anyhow!("unknown preset: `{name}`"))?;

        chain.push(name.clone());
        base = apply_with_presets(base, preset_config, resolve, chain, applied)?;
        chain.pop();
        applied.insert(name);
    }

    Ok(merge_json(base, config))
}

/// Extract and remove the `presets` array from a JSON value.
pub(super) fn extract_presets(value: &mut serde_json::Value) -> Vec<String> {
    let Some(obj) = value.as_object_mut() else {
        return vec![];
    };
    let Some(serde_json::Value::Array(arr)) = obj.remove("presets") else {
        return vec![];
    };
    arr.into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

pub(super) fn parse_config(merged: serde_json::Value) -> anyhow::Result<Config> {
    let serde_json::Value::Object(map) = merged else {
        anyhow::bail!("config must be a TOML table");
    };

    let schema = smart_config::ConfigSchema::new(&Config::DESCRIPTION, "");
    let source = smart_config::Json::new("merged config", map);
    let repo = smart_config::ConfigRepository::new(&schema).with(source);
    let parser = repo.single::<Config>()?;
    match parser.parse() {
        Ok(config) => Ok(config),
        Err(errors) => Err(anyhow::anyhow!(format_error(
            "invalid configuration",
            errors,
        ))),
    }
}

/// Merge two JSON values with custom rules:
/// - Null overlay: base wins (null never overwrites)
/// - Arrays: concatenate
/// - Objects: recursive merge
/// - Primitives: overlay wins
/// - Type mismatch: overlay wins
pub(super) fn merge_json(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (base, overlay) {
        (base, Value::Null) => base,
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
