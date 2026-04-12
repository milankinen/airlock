use std::collections::HashSet;
use std::path::{Path, PathBuf};

use smart_config::DescribeConfig;

use crate::config::de::format_error;
use crate::config::{Config, presets};

const EXTENSIONS: &[&str] = &["toml", "json", "yaml", "yml"];

/// Load configuration from hierarchical config files.
///
/// Files are loaded in order (later overrides former):
/// 1. `~/.cache/airlock/config.<ext>`
/// 2. `~/.airlock.<ext>`
/// 3. `<project_root>/airlock.<ext>`
/// 4. `<project_root>/airlock.local.<ext>`
///
/// Supported formats: TOML, JSON, YAML. For each slot the first matching
/// extension (`toml` → `json` → `yaml` → `yml`) wins.
///
/// If the merged config contains a `presets` array, the named
/// presets are applied as base layers before the user config.
pub fn load(project_root: &Path) -> anyhow::Result<Config> {
    let home = dirs::home_dir().unwrap_or_default();
    let bases: [PathBuf; 4] = [
        home.join(".cache/airlock/config"),
        home.join(".airlock"),
        project_root.join("airlock"),
        project_root.join("airlock.local"),
    ];

    // 1. Create base config
    let base = serde_json::Value::Object(serde_json::Map::new());

    // 2. Load user config — for each slot, use the first extension found
    let mut user_config = serde_json::Value::Object(serde_json::Map::new());
    for base_path in &bases {
        if let Some(value) = load_first(base_path)? {
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

/// Try each supported extension for `base` and parse the first file found.
fn load_first(base: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    for ext in EXTENSIONS {
        let path = base.with_extension(ext);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let value = parse_file(&path, &content)?;
        return Ok(Some(value));
    }
    Ok(None)
}

fn parse_file(path: &Path, content: &str) -> anyhow::Result<serde_json::Value> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => {
            toml::from_str(content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        Some("json") => {
            serde_json::from_str(content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        Some("yaml" | "yml") => {
            serde_yaml::from_str(content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        _ => anyhow::bail!("unsupported config format: {}", path.display()),
    }
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
    let config = match parser.parse() {
        Ok(config) => config,
        Err(errors) => {
            return Err(anyhow::anyhow!(format_error(
                "invalid configuration",
                errors,
            )));
        }
    };

    #[cfg(not(target_os = "linux"))]
    if config.vm.kvm {
        anyhow::bail!("kvm is only supported on Linux");
    }

    Ok(config)
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
