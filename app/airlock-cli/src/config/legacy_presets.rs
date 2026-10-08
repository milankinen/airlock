//! Legacy `presets` list support (`presets = ["python", "rust"]`).
//!
//! Older config files enable presets with a list of names. This module
//! keeps these files working as released. It checks the names, applies the
//! released preset documents below all config files, and maps the old
//! names to the `[packs]` table names for hints.

use std::collections::HashSet;

use include_dir::{Dir, include_dir};
use serde_json::{Map, Value};

use crate::config::merge::{merge_json, normalize_env};
use crate::packs::Pack;

/// The released documents. The 11 released names are the files
/// `src/config/presets/<name>.toml`, with the same settings as released
/// (`docker` is a later addition). Their comments can change, their
/// settings cannot. `build.rs` runs again when they change.
static DOCUMENTS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/src/config/presets");

/// Each released name and the `[packs]` table name of its pack, in the
/// order that error messages list them. `arch`, `fedora` and `suse` have no
/// pack any more.
const RELEASED_NAMES: [(&str, &str); 12] = [
    ("claude-code", "claude"),
    ("openai-codex", "codex"),
    ("copilot-cli", "copilot"),
    ("nodejs", "nodejs"),
    ("python", "python"),
    ("rust", "rust"),
    ("docker", "docker"),
    ("alpine", "alpine"),
    ("arch", "arch"),
    ("debian", "debian"),
    ("fedora", "fedora"),
    ("suse", "suse"),
];

/// Version that a `[packs]` table entry cannot have, because the list form
/// replaces it.
pub(crate) const LEGACY_VERSION: &str = "legacy";

/// Check that each name of a `presets` list is a released name.
/// Args:
///  - `origin`: The config file, for error messages
///  - `names`: Names of the list
///  - `known`: Built-in packs. For the name of a known pack, the error
///    shows its `[packs]` table entry.
pub(crate) fn validate_names(origin: &str, names: &[String], known: &[Pack]) -> anyhow::Result<()> {
    let released: Vec<&str> = RELEASED_NAMES.iter().map(|(name, _)| *name).collect();
    let Some(name) = names.iter().find(|name| !released.contains(&name.as_str())) else {
        return Ok(());
    };
    let is_table_name = known.iter().any(|p| p.metadata().name == *name);
    let hint = if is_table_name {
        format!(
            "; newer packs use the [packs] table of a project config file, for example \
             `[packs] {name} = {{ version = 1 }}`"
        )
    } else {
        String::new()
    };
    anyhow::bail!(
        "{origin}: unknown preset `{name}` (known: {}){hint}",
        released.join(", ")
    )
}

/// Remove `presets` from a config value and check that it is a list of
/// strings. A list is the only form that is accepted now.
/// Args:
///  - `value`: Config value of a file
///  - `origin`: The config file, at the start of error messages
///
/// Returns:
///   The names, or `None` if there is no `presets` key. `null` (for example
///   a YAML `presets:` with no value) counts as no key, the same as
///   [`merge_json`] does for nulls. A table is an error with a hint to use a
///   `[packs]` table.
pub(crate) fn take_presets_key(
    value: &mut Value,
    origin: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    let Some(presets) = value.as_object_mut().and_then(|obj| obj.remove("presets")) else {
        return Ok(None);
    };
    match presets {
        Value::Null => Ok(None),
        Value::Array(items) => {
            let names = items
                .into_iter()
                .map(|item| match item {
                    Value::String(name) => Ok(name),
                    other => Err(anyhow::anyhow!(
                        "{origin}: `presets` list entries must be preset names (strings), \
                         not {other}"
                    )),
                })
                .collect::<anyhow::Result<_>>()?;
            Ok(Some(names))
        }
        Value::Object(_) => anyhow::bail!(
            "{origin}: `presets` must be a list of preset names; use a [packs] table in a \
             project config file for versioned, installable packs"
        ),
        other => anyhow::bail!("{origin}: `presets` must be a list of preset names, not {other}"),
    }
}

/// Check if `name` is a released name.
pub(crate) fn is_released_name(name: &str) -> bool {
    RELEASED_NAMES.iter().any(|(released, _)| *released == name)
}

/// Get the first released name for a `[packs]` table name.
/// Returns:
///   The released name, or `None` if there is none.
pub(crate) fn released_name(table_name: &str) -> Option<&'static str> {
    RELEASED_NAMES
        .iter()
        .find(|(_, table)| *table == table_name)
        .map(|(released, _)| *released)
}

/// Merge the preset documents into one value, as released.
/// Args:
///  - `names`: Validated names of all lists, in file order
///
/// Returns:
///   The merged value. Each name applies once. The names of the `presets`
///   list of a document apply before the document. The env of each
///   document is normalized.
pub(crate) fn expand<'a>(names: impl IntoIterator<Item = &'a str>) -> anyhow::Result<Value> {
    let mut base = Value::Object(Map::new());
    let mut applied = HashSet::new();
    for name in names {
        base = apply(base, name, &mut vec![], &mut applied)?;
    }
    Ok(base)
}

/// Apply the document `name` onto `base`, its nested names first.
/// `chain` contains the documents that are in progress, `applied` the
/// documents that are done.
fn apply(
    mut base: Value,
    name: &str,
    chain: &mut Vec<String>,
    applied: &mut HashSet<String>,
) -> anyhow::Result<Value> {
    if applied.contains(name) {
        tracing::debug!("config: preset `{name}` already applied, skipping");
        return Ok(base);
    }
    if chain.iter().any(|c| c == name) {
        anyhow::bail!(
            "circular preset dependency: {} -> {name}",
            chain.join(" -> ")
        );
    }
    let mut document = document(name)?;
    let nested: Vec<String> = match document.as_object_mut().and_then(|d| d.remove("presets")) {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(String::from))
            .collect(),
        _ => vec![],
    };
    normalize_env(&mut document);

    tracing::debug!("config: applying preset `{name}`");
    chain.push(name.to_string());
    for nested_name in &nested {
        base = apply(base, nested_name, chain, applied)?;
    }
    chain.pop();
    applied.insert(name.to_string());
    Ok(merge_json(base, document))
}

/// Parse the document of the released name `name`.
fn document(name: &str) -> anyhow::Result<Value> {
    let file = DOCUMENTS
        .get_file(format!("{name}.toml"))
        .ok_or_else(|| anyhow::anyhow!("unknown preset: `{name}`"))?;
    let text = file
        .contents_utf8()
        .ok_or_else(|| anyhow::anyhow!("preset `{name}` is not UTF-8"))?;
    toml::from_str(text).map_err(|e| anyhow::anyhow!("preset `{name}`: {e}"))
}

#[cfg(test)]
mod tests {
    //! Tests for the list of released preset names.

    use super::*;

    /// Test that each released preset name has a document, that each document
    /// has a name, and that each name maps to a built-in pack.
    ///   1. Compare the document files with the released names
    ///   2. Check that each table name is a built-in pack, or a distro without a
    ///      pack
    #[test]
    fn released_names_match_documents_and_builtin_packs() {
        let mut files: Vec<String> = DOCUMENTS
            .files()
            .map(|f| f.path().display().to_string())
            .collect();
        files.sort_unstable();
        let mut expected: Vec<String> = RELEASED_NAMES
            .iter()
            .map(|(name, _)| format!("{name}.toml"))
            .collect();
        expected.sort_unstable();
        assert_eq!(files, expected);
        let packs = crate::packs::init().unwrap().builtin();
        for (_, table_name) in RELEASED_NAMES {
            assert!(
                ["arch", "fedora", "suse"].contains(&table_name)
                    || packs.iter().any(|p| p.metadata().name == table_name),
                "{table_name}"
            );
        }
    }
}
