//! The `packs` value of the config files.
//!
//! Each file's `[packs]` table is read on its own ([`read_layer_packs`]):
//! an entry is `<name> = { version = "1", enabled = false, args = { … } }`,
//! each key optional. The list form (`presets = ["python"]`) is plain
//! config ([`crate::config::legacy_presets`]). The entries of all files
//! then merge per pack ([`merge_pack_entries`]) and configure the version
//! they end up with ([`configure_packs`]).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::config::legacy_presets;
use crate::packs::{ConfiguredPack, Pack, PackManager};

/// The keys of an entry.
const ENTRY_KEYS: &str = "version, enabled, args";

/// One file's entry for one pack.
#[derive(Clone, Debug)]
pub struct PackEntry {
    /// The pack name (the key in the `[packs]` table).
    pub name: String,
    /// The file that holds the entry.
    pub file: String,
    pub enabled: Option<bool>,
    /// Normalized to a string (`1` → `"1"`).
    pub version: Option<String>,
    /// The `args` table as written; checked after the merge, against the
    /// final version.
    pub args: Map<String, Value>,
}

/// Remove the `[packs]` table from the file `origin`'s `value` and read
/// its entries; `known` are the packs that an entry can name. Problems
/// with single entries go to `problems`.
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

/// A `version` value as a string: a string as it is, a whole number ≥ 1
/// as its decimal text.
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
    pub name: String,
    /// The entries in file order (each file contributes one at most).
    pub contributors: Vec<PackEntry>,
}

impl MergedEntry {
    /// `enabled` of the highest file that sets it; true by default.
    pub fn enabled(&self) -> bool {
        self.contributors
            .iter()
            .rev()
            .find_map(|c| c.enabled)
            .unwrap_or(true)
    }

    /// The highest file's entry that sets `version`.
    fn version_source(&self) -> Option<&PackEntry> {
        self.contributors.iter().rev().find(|c| c.version.is_some())
    }

    /// The contributing files, for error messages.
    fn files(&self) -> String {
        let files: Vec<&str> = self.contributors.iter().map(|c| c.file.as_str()).collect();
        files.join(", ")
    }
}

/// Group the entries of all files (in file order) by pack.
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

/// Merge the entries of all files and configure the enabled ones at their
/// version. `known` are the packs of `packs` that an entry can name; the
/// configured packs are in their order. Problems go to `problems`.
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

/// Configure a merged entry of `newest` (the newest version of the pack):
/// its version, and the args of the files whose entry has that version or
/// none. Per arg the highest of these files wins; each file's value must
/// fit the arg. Args written for another version are dropped.
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

    // Per arg key: each file that sets it, with its value, lowest first.
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

/// The versions of the pack of `newest` that `packs` has, newest first.
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
