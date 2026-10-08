//! `[packs]` tables of the config files.
//!
//! Reads the `[packs]` table of each config file, combines the entries for
//! the same pack from all files, and makes the configured packs from them.
//! The legacy `presets` list is a different feature.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::config::legacy_presets;
use crate::packs::{ConfiguredPack, Pack, PackManager};

/// Keys of an entry, for error messages.
const ENTRY_KEYS: &str = "version, enabled, args";

/// The entry of one file for one pack:
/// `<name> = { version = "1", enabled = false, args = { … } }`. All keys
/// are optional.
#[derive(Clone, Debug)]
pub struct PackEntry {
    /// Pack name (the key in the `[packs]` table).
    pub name: String,
    /// The file that contains the entry.
    pub file: String,
    /// The `enabled` value, if set.
    pub enabled: Option<bool>,
    /// The `version` value as a string (`1` → `"1"`), if set.
    pub version: Option<String>,
    /// The `args` table as written. It is checked after the merge, against
    /// the final version.
    pub args: Map<String, Value>,
}

/// Remove the `[packs]` table from the value of a file and read its entries.
/// Args:
///  - `origin`: The config file, for error messages
///  - `value`: Config value of the file
///  - `known`: Packs that an entry can name
///  - `problems`: Receives the problems with entries, one line each
///
/// Returns:
///   The entries of known packs that are tables. A field with a value
///   that is not valid is left unset.
pub fn read_layer_packs(
    origin: &str,
    value: &mut Value,
    known: &[Pack],
    problems: &mut Vec<String>,
) -> Vec<PackEntry> {
    match value.as_object_mut().and_then(|obj| obj.remove("packs")) {
        Some(Value::Object(table)) => read_table(origin, table, known, problems),
        Some(other) => {
            problems.push(format!(
                "* `packs` must be a table, not {other} (set in: {origin})"
            ));
            vec![]
        }
        None => vec![],
    }
}

/// Read the entries of a `[packs]` table.
fn read_table(
    origin: &str,
    table: Map<String, Value>,
    known: &[Pack],
    problems: &mut Vec<String>,
) -> Vec<PackEntry> {
    let mut entries = Vec::new();
    for (name, raw) in table {
        if !known.iter().any(|p| p.metadata().name == name) {
            let known: Vec<&str> = known.iter().map(|p| p.metadata().name.as_str()).collect();
            let hint = if legacy_presets::is_released_name(&name) {
                format!("; `{name}` is a list-form name: write `presets = [\"{name}\"]`")
            } else {
                String::new()
            };
            problems.push(format!(
                "* `packs.{name}` unknown pack (known: {}){hint} (set in: {origin})",
                known.join(", ")
            ));
            continue;
        }
        let Value::Object(mut entry) = raw else {
            problems.push(format!(
                "* `packs.{name}` must be a table (set in: {origin})"
            ));
            continue;
        };
        let enabled = match entry.remove("enabled") {
            None => None,
            Some(Value::Bool(b)) => Some(b),
            Some(_) => {
                problems.push(format!(
                    "* `packs.{name}.enabled` must be true or false (set in: {origin})"
                ));
                None
            }
        };
        let version = match entry.remove("version").map(|v| normalize_version(&v)) {
            None => None,
            Some(Ok(version)) => Some(version),
            Some(Err(e)) => {
                problems.push(format!("* `packs.{name}.version` {e} (set in: {origin})"));
                None
            }
        };
        let args = match entry.remove("args") {
            None => Map::new(),
            Some(Value::Object(args)) => args,
            Some(_) => {
                problems.push(format!(
                    "* `packs.{name}.args` must be a table (set in: {origin})"
                ));
                Map::new()
            }
        };
        for key in entry.keys() {
            problems.push(format!(
                "* `packs.{name}.{key}` unknown key (known: {ENTRY_KEYS}; args go in \
                 `args = {{ {key} = … }}`) (set in: {origin})"
            ));
        }
        entries.push(PackEntry {
            name,
            file: origin.to_string(),
            enabled,
            version,
            args,
        });
    }
    entries
}

/// Convert a `version` value to a string. A string stays as it is. A whole
/// number ≥ 1 becomes its decimal text.
fn normalize_version(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => match (n.as_u64(), n.as_i64()) {
            (Some(n), _) if n >= 1 => Ok(n.to_string()),
            (Some(_), _) | (None, Some(_)) => Err("must be 1 or higher".into()),
            (None, None) => Err(format!("must be a string or a whole number, not {n}")),
        },
        other => Err(format!("must be a string or a whole number, not {other}")),
    }
}

/// The entries of one pack from all files, lowest precedence first.
pub struct MergedEntry {
    /// Pack name.
    pub name: String,
    /// The entries in file order (at most one from each file).
    pub contributors: Vec<PackEntry>,
}

impl MergedEntry {
    /// Get `enabled` from the highest file that sets it. True by default.
    pub fn enabled(&self) -> bool {
        self.contributors
            .iter()
            .rev()
            .find_map(|c| c.enabled)
            .unwrap_or(true)
    }

    /// The entry of the highest file that sets `version`.
    fn version_source(&self) -> Option<&PackEntry> {
        self.contributors.iter().rev().find(|c| c.version.is_some())
    }

    /// List the files of the entries, for error messages.
    fn files(&self) -> String {
        let files: Vec<&str> = self.contributors.iter().map(|c| c.file.as_str()).collect();
        files.join(", ")
    }
}

/// Group the entries of all files by pack.
/// Args:
///  - `entries`: Entries of all files, in file order
///
/// Returns:
///   One merged entry for each pack, in order of the first entry.
pub fn merge_pack_entries(entries: Vec<PackEntry>) -> Vec<MergedEntry> {
    let mut merged: Vec<MergedEntry> = Vec::new();
    for entry in entries {
        match merged.iter_mut().find(|m| m.name == entry.name) {
            Some(m) => m.contributors.push(entry),
            None => merged.push(MergedEntry {
                name: entry.name.clone(),
                contributors: vec![entry],
            }),
        }
    }
    merged
}

/// Merge the entries of all files and configure the enabled packs at their
/// version.
/// Args:
///  - `entries`: Entries of all files, in file order
///  - `packs`: Known packs
///  - `known`: Newest version of each pack that an entry can name
///  - `problems`: Receives the problems, one line each
///
/// Returns:
///   The configured packs, in the order of `known`.
pub async fn configure_packs(
    entries: Vec<PackEntry>,
    packs: &PackManager,
    known: &[Pack],
    problems: &mut Vec<String>,
) -> Vec<ConfiguredPack> {
    let merged = merge_pack_entries(entries);
    let mut configured = Vec::new();
    for pack in known {
        let Some(entry) = merged.iter().find(|m| m.name == pack.metadata().name) else {
            continue;
        };
        if !entry.enabled() {
            continue;
        }
        match configure_entry(entry, packs, pack).await {
            Ok(pack) => configured.push(pack),
            Err(mut errs) => problems.append(&mut errs),
        }
    }
    configured
}

/// Configure a merged entry at its version.
///
/// Uses the args of the files whose entry has that version or no version.
/// For each arg, the highest of these files wins. The value of each file
/// must be valid for the arg. Args written for a different version are
/// dropped.
/// Args:
///  - `entry`: Merged entry of the pack
///  - `packs`: Known packs
///  - `newest`: Newest version of the pack
///
/// Returns:
///   The configured pack, or the problems, one line each.
async fn configure_entry(
    entry: &MergedEntry,
    packs: &PackManager,
    newest: &Pack,
) -> Result<ConfiguredPack, Vec<String>> {
    let name = &newest.metadata().name;
    let Some(source) = entry.version_source() else {
        return Err(vec![format!(
            "* `packs.{name}` needs `version` (set in: {}): {}",
            entry.files(),
            supported_versions(packs, newest).await.join(" or ")
        )]);
    };
    let version = source.version.clone().expect("the source sets version");
    let Some(pack) = packs.resolve(name, &version).await else {
        if version == legacy_presets::LEGACY_VERSION
            && let Some(released) = legacy_presets::released_name(name)
        {
            return Err(vec![format!(
                "* `packs.{name}`: version \"{version}\" is not supported; use the list \
                 form `presets = [\"{released}\"]` (set in: {})",
                entry.files()
            )]);
        }
        let supported: Vec<String> = supported_versions(packs, newest)
            .await
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect();
        return Err(vec![format!(
            "* `packs.{name}`: version \"{version}\" is not supported (supported: {}) \
             (set in: {})",
            supported.join(", "),
            entry.files()
        )]);
    };

    // For each arg key: each file that sets it, with its value, lowest first.
    let mut set_by: BTreeMap<&str, Vec<(&str, &Value)>> = BTreeMap::new();
    for c in &entry.contributors {
        if let Some(other) = c.version.as_ref().filter(|v| **v != version) {
            if !c.args.is_empty() {
                tracing::debug!(
                    "config: {}: the args of packs.{name} for version {other} are dropped \
                     (the version is {version})",
                    c.file
                );
            }
            continue;
        }
        for (key, raw) in &c.args {
            set_by
                .entry(key.as_str())
                .or_default()
                .push((c.file.as_str(), raw));
        }
    }
    let mut problems = Vec::new();
    let mut args = BTreeMap::new();
    for (key, sources) in set_by {
        let path = format!("`packs.{name}.args.{key}`");
        let Some(arg) = pack.args().iter().find(|arg| arg.key == key) else {
            let files: Vec<&str> = sources.iter().map(|(file, _)| *file).collect();
            problems.push(format!(
                "* {path} unknown arg of version \"{version}\" ({}) (set in: {})",
                pack.known_args(),
                files.join(", ")
            ));
            continue;
        };
        for (file, raw) in sources {
            match arg.kind.parse(raw) {
                Ok(value) => {
                    args.insert(key.to_string(), value);
                }
                Err(e) => problems.push(format!("* {path} {e} (set in: {file})")),
            }
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(pack.configure(&args))
}

/// List the available versions of the pack of `newest`, newest first.
/// Versions are the whole numbers from 1.
async fn supported_versions(packs: &PackManager, newest: &Pack) -> Vec<String> {
    let metadata = newest.metadata();
    let Ok(highest) = metadata.version.parse::<u64>() else {
        return vec![metadata.version.clone()];
    };
    let mut versions = Vec::new();
    for version in (1..=highest).rev().map(|v| v.to_string()) {
        if packs.resolve(&metadata.name, &version).await.is_some() {
            versions.push(version);
        }
    }
    versions
}
