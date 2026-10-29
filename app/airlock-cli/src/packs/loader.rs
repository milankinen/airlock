//! Pack loader.
//!
//! Reads the files of one pack version and checks that the pack is valid.
//! The loader does not know where the files come from. Each pack source
//! gives it the file names and contents of one pack directory.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::config::files::{EXTENSIONS, parse_file};
use crate::packs::id::is_identifier;
use crate::packs::{
    ArgKind, PackArg, PackConfig, PackId, PackKind, PackMetadata, PackSource, PackVersionData,
};

/// File name of the pack description.
pub(super) const PACK_FILE: &str = "pack.toml";
const LUA_CONFIG_FILE: &str = "config.lua";
const SETUP_FILE: &str = "setup.sh";

/// Keys of a `[packs]` entry. No arg can use them.
const RESERVED_ARG_KEYS: [&str; 3] = ["version", "enabled", "args"];

/// Get the names of the files that a pack directory can have.
pub(super) fn file_names() -> impl Iterator<Item = String> {
    [PACK_FILE, LUA_CONFIG_FILE, SETUP_FILE]
        .into_iter()
        .map(String::from)
        .chain(EXTENSIONS.iter().map(|ext| format!("config.{ext}")))
}

/// `pack.toml` of a pack directory.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackFile {
    label: String,
    /// One-line description of the version (see
    /// [`PackMetadata::description`]). The loader checks that it is not
    /// empty.
    description: String,
    /// What the pack is for. A pack with no kind is [`PackKind::Other`].
    kind: Option<PackKind>,
    #[serde(default)]
    args: Vec<ArgEntry>,
}

/// One `[[args]]` entry of `pack.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArgEntry {
    key: String,
    r#type: ArgType,
    description: String,
    default: Value,
    /// Valid values of a choice arg.
    values: Option<Vec<String>>,
    /// For a choice arg: if true, any non-empty string is also valid.
    other: Option<bool>,
}

/// The `type` field of an `[[args]]` entry.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ArgType {
    Bool,
    Choice,
}

/// Load and validate the files of one pack version.
///
/// A pack directory contains:
///  * `pack.toml` (label, description, kind and args). Required.
///  * At most one config file `config.{toml,json,yaml,yml,lua}`.
///  * The setup script `setup.sh`.
///
/// Any other file is an error.
/// Args:
///  - `at`: Path of the pack directory, for error messages, for example
///    `packs/claude@1`
///  - `id`: Pack id
///  - `version`: Pack version, or `None` for a pack with no versions
///  - `files`: Contents of the regular files of the directory, by file
///    name. The caller removes hidden files and editor backups (see
///    [`is_skipped`]), or gives only the known files (see [`file_names`]).
///
/// Returns:
///   The pack version, or an error that names the problem. Its source is
///   [`PackSource::Builtin`] and it has no content hash and no snapshot: a
///   pack source from the disk sets them.
pub(super) fn load_pack(
    at: &str,
    id: PackId,
    version: Option<String>,
    files: &BTreeMap<&str, &[u8]>,
) -> anyhow::Result<PackVersionData> {
    let at_file = |file_name: &str| format!("{at}/{file_name}");
    let mut used: HashSet<&str> = HashSet::new();

    let pack_at = at_file(PACK_FILE);
    let pack_file = files
        .get(PACK_FILE)
        .with_context(|| crate::messages::packs::pack_file_missing(&pack_at))?;
    used.insert(PACK_FILE);
    let pack: PackFile =
        toml::from_str(utf8(&pack_at, pack_file)?).with_context(|| pack_at.clone())?;
    ensure!(
        !pack.description.trim().is_empty(),
        crate::messages::packs::pack_description_empty(&pack_at)
    );
    let args = load_args(&pack_at, pack.args)?;

    let config_files: Vec<&str> = files
        .keys()
        .copied()
        .filter(|file_name| {
            *file_name == LUA_CONFIG_FILE
                || EXTENSIONS
                    .iter()
                    .any(|ext| *file_name == format!("config.{ext}"))
        })
        .collect();
    ensure!(
        config_files.len() <= 1,
        crate::messages::packs::pack_config_files_many(at, config_files.join(", "))
    );
    let config = match config_files.first() {
        Some(&LUA_CONFIG_FILE) => {
            used.insert(LUA_CONFIG_FILE);
            let path = at_file(LUA_CONFIG_FILE);
            Some(PackConfig::Lua(utf8(&path, files[LUA_CONFIG_FILE])?.into()))
        }
        Some(&file_name) => {
            used.insert(file_name);
            Some(PackConfig::Static(document(
                &at_file(file_name),
                files[file_name],
            )?))
        }
        None => None,
    };

    let setup = match files.get(SETUP_FILE) {
        Some(bytes) => {
            used.insert(SETUP_FILE);
            // Refuse a large script here, not at the install exec.
            let max = super::install::compose::max_setup_len();
            if bytes.len() > max {
                bail!(crate::messages::packs::setup_too_large(
                    at_file(SETUP_FILE),
                    max
                ));
            }
            Some(utf8(&at_file(SETUP_FILE), bytes)?.into())
        }
        None => None,
    };

    if let Some(file_name) = files.keys().find(|file_name| !used.contains(*file_name)) {
        bail!(crate::messages::packs::pack_file_unknown(at_file(
            file_name
        )));
    }

    Ok(PackVersionData {
        metadata: PackMetadata {
            id,
            version,
            label: pack.label,
            description: pack.description,
            kind: pack.kind.unwrap_or(PackKind::Other),
            has_setup: setup.is_some(),
            source: PackSource::Builtin,
        },
        args,
        config,
        setup,
        content_hash: None,
        snapshot: None,
    })
}

/// Check the `[[args]]` entries of a `pack.toml` and convert them.
/// Args:
///  - `at`: Path of the file, for error messages
///  - `entries`: The `[[args]]` entries
///
/// Returns:
///   The args, or an error if a key is not valid or not unique, a field
///   does not fit the arg type, `values` is empty or has a duplicate, or
///   the default is not valid.
fn load_args(at: &str, entries: Vec<ArgEntry>) -> anyhow::Result<Vec<PackArg>> {
    let mut args: Vec<PackArg> = Vec::new();
    for entry in entries {
        let arg_at = crate::messages::packs::arg_at(at, &entry.key);
        ensure!(
            is_identifier(&entry.key) && !RESERVED_ARG_KEYS.contains(&entry.key.as_str()),
            crate::messages::packs::arg_key_invalid(&arg_at)
        );
        ensure!(
            !args.iter().any(|arg| arg.key == entry.key),
            crate::messages::packs::arg_key_duplicate(&arg_at)
        );
        let kind = match (entry.r#type, entry.values) {
            (ArgType::Bool, None) if entry.other.is_none() => ArgKind::Bool,
            (ArgType::Bool, _) => {
                bail!(crate::messages::packs::arg_values_only_for_choice(&arg_at))
            }
            (ArgType::Choice, None) => {
                bail!(crate::messages::packs::arg_choice_needs_values(&arg_at))
            }
            (ArgType::Choice, Some(values)) => {
                ensure!(
                    !values.is_empty(),
                    crate::messages::packs::arg_values_empty(&arg_at)
                );
                let mut seen = HashSet::new();
                for value in &values {
                    ensure!(
                        seen.insert(value.as_str()),
                        crate::messages::packs::arg_value_duplicate(&arg_at, value)
                    );
                }
                ArgKind::Choice {
                    values,
                    other: entry.other.unwrap_or(false),
                }
            }
        };
        let default = kind
            .parse(&entry.default)
            .map_err(|e| anyhow!(crate::messages::packs::arg_default_invalid(&arg_at, e)))?;
        args.push(PackArg {
            key: entry.key,
            description: entry.description,
            kind,
            default,
        });
    }
    Ok(args)
}

/// Parse a static config file of a pack directory. The file must be a table
/// that does not set `packs` or `presets`.
fn document(path: &str, bytes: &[u8]) -> anyhow::Result<Value> {
    let value = parse_file(path, utf8(path, bytes)?)?;
    ensure!(
        value.is_object(),
        crate::messages::packs::pack_config_file_not_table(path)
    );
    ensure!(
        value.get("packs").is_none() && value.get("presets").is_none(),
        crate::messages::packs::pack_config_file_reserved_keys(path)
    );
    Ok(value)
}

/// The contents of the file at `path` as UTF-8.
fn utf8<'a>(path: &str, bytes: &'a [u8]) -> anyhow::Result<&'a str> {
    std::str::from_utf8(bytes)
        .map_err(|_| anyhow!(crate::messages::packs::pack_file_not_utf8(path)))
}

/// Check if the file is hidden or an editor backup (`.DS_Store`,
/// `config.toml~`). Pack sources ignore these files.
pub(super) fn is_skipped(file_name: &str) -> bool {
    file_name.starts_with('.') || file_name.ends_with('~')
}
