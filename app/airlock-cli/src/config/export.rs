//! Config export.
//!
//! Makes config documents that the user can save as a config file:
//!  * the project config: the project files and the local project file,
//!    without the user files
//!  * the resolved config: the final config values after all layers and
//!    packs merge
//!
//! The documents can be TOML, JSON or YAML.

use clap::ValueEnum;
use serde_json::Value;

use super::{LayeredConfig, ResolvedConfig, merge_json};

/// Output format of an exported config.
#[derive(ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Toml,
    Json,
    Yaml,
}

impl LayeredConfig {
    /// Merge the project files and the local project file into one document.
    /// The user files are not in it. The `[packs]` tables stay as they are,
    /// and the packs do not merge in.
    /// Returns:
    ///   The document, or `None` if the project has no config (see
    ///   [`Self::has_project_config`]).
    pub fn project_document(&self) -> Option<Value> {
        if !self.has_project_config() {
            return None;
        }
        let layers = || self.local.iter().chain(&self.project);
        let mut merged = layers().fold(Value::Object(serde_json::Map::new()), |merged, layer| {
            merge_json(merged, layer.value.clone())
        });
        let presets: Vec<&String> = layers()
            .filter_map(|layer| layer.legacy_presets.as_ref())
            .flatten()
            .collect();
        if !presets.is_empty() {
            merged["presets"] = serde_json::json!(presets);
        }
        if let Some(env) = merged.get_mut("env").and_then(Value::as_object_mut) {
            env.values_mut().for_each(plain_env_entry);
        }
        Some(merged)
    }
}

/// Make the resolved config document of `resolved`: the final config
/// values. The packs are merged in already, thus the document has no
/// `[packs]` table. The `--network` policy is not in it, because it
/// applies to one run only.
/// Empty tables and plain env entries are in their short form, thus the
/// document is easier to read.
pub fn resolved_document(resolved: &ResolvedConfig) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(&resolved.values)?;
    value["network"]["policy"] = serde_json::to_value(resolved.configured_policy)?;
    drop_nulls(&mut value);
    drop_empty_tables(&mut value);
    if let Some(env) = value.get_mut("env").and_then(Value::as_object_mut) {
        env.values_mut().for_each(plain_env_entry);
    }
    Ok(value)
}

/// Write `document` as text in `format`. Null values are not set values,
/// thus the text does not have them.
/// Returns:
///   The text, or an error if `format` cannot hold the document.
pub fn render(document: &Value, format: Format) -> anyhow::Result<String> {
    let mut document = document.clone();
    drop_nulls(&mut document);
    Ok(match format {
        Format::Toml => toml::to_string_pretty(&document)?,
        Format::Json => serde_json::to_string_pretty(&document)? + "\n",
        Format::Yaml => serde_yaml::to_string(&document)?,
    })
}

/// Change an `[env]` entry that has only a value back to its plain-string
/// form (the reverse of [`super::merge::normalize_env`]). An unmasked
/// entry with no other keys is also plain.
fn plain_env_entry(entry: &mut Value) {
    let Some(map) = entry.as_object() else {
        return;
    };
    let plain = map.iter().all(|(key, value)| match key.as_str() {
        "value" => value.is_string(),
        "mask" => value == &Value::Bool(false),
        _ => false,
    });
    if plain && let Some(Value::String(value)) = map.get("value") {
        *entry = Value::String(value.clone());
    }
}

/// Remove the empty tables from `value`. A missing table gets the same
/// defaults as an empty one.
fn drop_empty_tables(value: &mut Value) {
    if let Value::Object(map) = value {
        map.values_mut().for_each(drop_empty_tables);
        map.retain(|_, v| !v.as_object().is_some_and(serde_json::Map::is_empty));
    }
}

/// Remove the unset (null) values from `value`. TOML has no null.
fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(drop_nulls);
        }
        Value::Array(items) => {
            items.retain(|v| !v.is_null());
            items.iter_mut().for_each(drop_nulls);
        }
        _ => {}
    }
}
