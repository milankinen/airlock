//! `[packs]` tables of the config files.
//!
//! Reads the `[packs]` table of each config file, combines the entries for
//! the same pack from all files, and makes the configured packs from them.
//! The legacy `presets` list is a different feature.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::config::legacy_presets;
use crate::packs::id::toml_key;
use crate::packs::{ConfiguredPack, Pack, PackManager, PackRef};

/// Keys of an entry, for error messages.
const ENTRY_KEYS: &str = "version, enabled, args";

/// The entry of one file for one pack:
/// `<name> = { version = "1", enabled = false, args = { … } }`. All keys
/// are optional. The entry of a user pack (`"~name" = { … }`) or a project
/// pack (`"./path" = { … }`) cannot set `version`.
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
///  - `known`: Packs that an entry can name, except the project packs. The
///    project packs load later.
///  - `problems`: Receives the problems with entries, one line each
///
/// Returns:
///   The entries of known packs and of project packs that are tables. A
///   field with a value that is not valid is left unset.
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
                "* {}",
                crate::messages::config::packs_not_table(other, origin)
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
        let pack_ref = PackRef::parse(&name);
        // The messages show the key as TOML has it, for example `"~tools"`.
        let shown = toml_key(&name);
        if pack_ref.is_none() && name.starts_with('~') {
            problems.push(format!(
                "* {}",
                crate::messages::config::user_pack_name_invalid(&shown)
            ));
            continue;
        } else if pack_ref.is_none() && looks_like_path(&name) {
            problems.push(format!(
                "* {}",
                crate::messages::config::project_pack_path_invalid(&shown)
            ));
            continue;
        } else if !matches!(pack_ref, Some(PackRef::Project { .. }))
            // Project packs load later (see `add_project_packs`).
            && !known.iter().any(|p| p.metadata().id == name)
        {
            if let Some(PackRef::Local { .. }) = pack_ref {
                problems.push(format!(
                    "* {}",
                    crate::messages::config::unknown_user_pack(&shown)
                ));
                continue;
            }
            let known: Vec<&str> = known.iter().map(|p| p.metadata().id.as_str()).collect();
            let hint = if legacy_presets::is_released_name(&name) {
                crate::messages::config::pack_list_form_hint(&name)
            } else {
                String::new()
            };
            problems.push(format!(
                "* {}",
                crate::messages::config::unknown_pack(&shown, known.join(", "), hint, origin)
            ));
            continue;
        }
        let Value::Object(mut entry) = raw else {
            problems.push(format!(
                "* {}",
                crate::messages::config::pack_entry_not_table(&shown, origin)
            ));
            continue;
        };
        let enabled = match entry.remove("enabled") {
            None => None,
            Some(Value::Bool(b)) => Some(b),
            Some(_) => {
                problems.push(format!(
                    "* {}",
                    crate::messages::config::pack_enabled_not_bool(&shown, origin)
                ));
                None
            }
        };
        let version = match entry.remove("version").map(|v| normalize_version(&v)) {
            None => None,
            Some(_) if pack_ref.is_some_and(|r| !r.needs_version()) => {
                problems.push(format!(
                    "* {}",
                    crate::messages::config::pack_version_not_allowed(&shown)
                ));
                None
            }
            Some(Ok(version)) => Some(version),
            Some(Err(e)) => {
                problems.push(format!(
                    "* {}",
                    crate::messages::config::pack_version_invalid(&shown, e, origin)
                ));
                None
            }
        };
        let args = match entry.remove("args") {
            None => Map::new(),
            Some(Value::Object(args)) => args,
            Some(_) => {
                problems.push(format!(
                    "* {}",
                    crate::messages::config::pack_args_not_table(&shown, origin)
                ));
                Map::new()
            }
        };
        for key in entry.keys() {
            problems.push(format!(
                "* {}",
                crate::messages::config::pack_unknown_key(&shown, key, ENTRY_KEYS, origin)
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

/// Return true if a `[packs]` key that is not a valid pack id looks like a
/// path. The error then tells the form of a project pack path.
fn looks_like_path(name: &str) -> bool {
    name.starts_with('.') || name.contains('/')
}

/// Convert a `version` value to a string. A string stays as it is. A whole
/// number ≥ 1 becomes its decimal text.
fn normalize_version(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => match (n.as_u64(), n.as_i64()) {
            (Some(n), _) if n >= 1 => Ok(n.to_string()),
            (Some(_), _) | (None, Some(_)) => {
                Err(crate::messages::config::VERSION_BELOW_ONE.into())
            }
            (None, None) => Err(crate::messages::config::version_wrong_type(n)),
        },
        other => Err(crate::messages::config::version_wrong_type(other)),
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
        let Some(entry) = merged.iter().find(|m| pack.metadata().id == m.name) else {
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

/// Configure a merged entry at its version. A pack with no versions (a
/// user pack) uses the args of all files.
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
    let id = &newest.metadata().id;
    let name = toml_key(id.as_str());
    let name = name.as_ref();
    if !id.pack_ref().needs_version() {
        // A pack with no versions: all files give args. A `version` key is
        // an error in `read_table`.
        let pack = packs
            .resolve(id, None)
            .await
            .unwrap_or_else(|| newest.clone());
        return configure_args(name, entry.contributors.iter(), &pack, None);
    }
    let Some(source) = entry.version_source() else {
        return Err(vec![format!(
            "* {}",
            crate::messages::config::pack_needs_version(
                name,
                entry.files(),
                supported_versions(packs, newest).await.join(" or ")
            )
        )]);
    };
    let version = source.version.clone().expect("the source sets version");
    let Some(pack) = packs.resolve(id, Some(&version)).await else {
        if version == legacy_presets::LEGACY_VERSION
            && let Some(released) = legacy_presets::released_name(name)
        {
            return Err(vec![format!(
                "* {}",
                crate::messages::config::pack_version_use_list_form(
                    name,
                    &version,
                    released,
                    entry.files()
                )
            )]);
        }
        let supported: Vec<String> = supported_versions(packs, newest)
            .await
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect();
        return Err(vec![format!(
            "* {}",
            crate::messages::config::pack_version_unsupported(
                name,
                &version,
                supported.join(", "),
                entry.files()
            )
        )]);
    };
    let contributors = entry.contributors.iter().filter(|c| {
        let Some(other) = c.version.as_ref().filter(|v| **v != version) else {
            return true;
        };
        if !c.args.is_empty() {
            tracing::debug!(
                "config: {}: the args of packs.{name} for version {other} are dropped \
                 (the version is {version})",
                c.file
            );
        }
        false
    });
    configure_args(name, contributors, &pack, Some(&version))
}

/// Check the args of the entries and configure the pack with them. For
/// each arg, the highest entry wins. The value of each entry must be valid
/// for the arg.
/// Args:
///  - `name`: Pack id as a TOML key, for error messages
///  - `contributors`: The entries that give args, lowest first
///  - `pack`: The pack version
///  - `version`: Pack version, for error messages. `None` for a pack with
///    no versions.
///
/// Returns:
///   The configured pack, or the problems, one line each.
fn configure_args<'a>(
    name: &str,
    contributors: impl Iterator<Item = &'a PackEntry>,
    pack: &Pack,
    version: Option<&str>,
) -> Result<ConfiguredPack, Vec<String>> {
    // For each arg key: each file that sets it, with its value, lowest first.
    let mut set_by: BTreeMap<&str, Vec<(&str, &Value)>> = BTreeMap::new();
    for c in contributors {
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
            let problem = match version {
                Some(version) => {
                    let files: Vec<&str> = sources.iter().map(|(file, _)| *file).collect();
                    crate::messages::config::pack_unknown_arg(
                        &path,
                        version,
                        pack.known_args(),
                        files.join(", "),
                    )
                }
                None => {
                    crate::messages::config::pack_unknown_arg_no_version(&path, pack.known_args())
                }
            };
            problems.push(format!("* {problem}"));
            continue;
        };
        for (file, raw) in sources {
            match arg.kind.parse(raw) {
                Ok(value) => {
                    args.insert(key.to_string(), value);
                }
                Err(e) => problems.push(format!(
                    "* {}",
                    crate::messages::config::pack_arg_invalid(&path, e, file)
                )),
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
///
/// Returns:
///   The versions. Empty for a pack with no versions: the caller must
///   handle that case.
async fn supported_versions(packs: &PackManager, newest: &Pack) -> Vec<String> {
    let metadata = newest.metadata();
    let Some(newest_version) = &metadata.version else {
        return Vec::new();
    };
    let Ok(highest) = newest_version.parse::<u64>() else {
        return vec![newest_version.clone()];
    };
    let mut versions = Vec::new();
    for version in (1..=highest).rev().map(|v| v.to_string()) {
        if packs.resolve(&metadata.id, Some(&version)).await.is_some() {
            versions.push(version);
        }
    }
    versions
}
