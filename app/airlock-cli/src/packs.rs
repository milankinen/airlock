//! Packs: config that a project enables in `[packs]`, and for some
//! packs a setup script that installs into the sandbox.
//!
//! [`init`] loads the known packs into a [`PackManager`]. Each [`Pack`]
//! is one version of a pack (`name@version`), a folder
//! `packs/<name>@<version>/` of the repository ([`builtin`]): its
//! metadata and args, the config it applies and its setup script.
//! [`Pack::configure`] fills the defaults of the arg values of a config
//! and gives a [`ConfiguredPack`]: its config values (a static document,
//! or what its `config.lua` makes of the args, see [`lua_config`]) and
//! its install ([`InstallerScript`]).
//! [`crate::config::pack_entries`] reads the `[packs]` tables of the
//! config files. The released list form (`presets = ["python"]`) is plain
//! config ([`crate::config::legacy_presets`]).
//!
//! Versions copy their files rather than share them: a change of a
//! config or a setup script ships as a new version. So the name, the
//! version and the arg values define what a pack puts on the sandbox
//! disk (see [`ConfiguredPack::setup_installer`]).
//!
//! `airlock start` installs the configured packs that have a setup
//! script in an install boot ([`install::setup`]); [`install::state`]
//! records what is on the sandbox disk and [`install::plan`] decides what
//! to install.

mod builtin;
pub mod install;
pub mod lua_config;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Load the known packs: the built-in ones ([`builtin`]).
pub fn init() -> anyhow::Result<PackManager> {
    builtin::load(&builtin::BUILTIN_PACKS)
}

/// [`init`] with the test pack `sample@1` (see
/// [`builtin::load_with_sample`]).
#[cfg(test)]
pub fn init_with_sample() -> PackManager {
    builtin::load_with_sample()
}

/// The packs of an in-memory `packs/` directory: `files` are
/// `(<folder>/<file>, text)` (deeper paths make nested folders).
#[cfg(test)]
pub fn load_test_packs(files: Vec<(&'static str, String)>) -> anyhow::Result<PackManager> {
    builtin::load(builtin::fixture(vec![], files))
}

/// The known packs. Cheap to clone.
#[derive(Clone)]
pub struct PackManager {
    /// Every version of every pack: the packs by kind (see [`PackKind`]),
    /// then by name; the versions of one pack oldest first.
    packs: Arc<[Pack]>,
}

impl PackManager {
    /// The newest version of each built-in pack, by kind, then by name
    /// (the order of documents, installs and listings).
    pub fn builtin(&self) -> Vec<Pack> {
        let mut newest: Vec<Pack> = Vec::new();
        for pack in self.packs.iter() {
            match newest.last_mut() {
                Some(last) if last.metadata().name == pack.metadata().name => {
                    *last = pack.clone();
                }
                _ => newest.push(pack.clone()),
            }
        }
        newest
    }

    /// The pack `name` at `version`, if there is one.
    #[allow(
        clippy::unused_async,
        reason = "remote packs resolve over the network later"
    )]
    pub async fn resolve(&self, name: &str, version: &str) -> Option<Pack> {
        self.packs
            .iter()
            .find(|p| p.metadata().name == name && p.metadata().version == version)
            .cloned()
    }
}

/// One version of a pack (`name@version`), with everything it needs.
/// Cheap to clone.
#[derive(Clone)]
pub struct Pack(Arc<PackVersionData>);

/// What names and describes a pack version.
pub struct PackMetadata {
    /// Key in the `[packs]` table.
    pub name: String,
    /// `"1"`, `"2"`, …
    pub version: String,
    /// Human-readable name.
    pub label: String,
    /// What the version does, in one line; the setup wizard shows it.
    pub description: String,
    pub kind: PackKind,
    /// Whether the version has a setup script (`setup.sh`).
    pub has_setup: bool,
}

/// What a pack is for. The packs are ordered by kind, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackKind {
    /// A Linux distribution for the sandbox.
    Distro,
    /// A coding agent.
    Agent,
    /// A tool or a language.
    Tool,
}

/// The data of one pack version.
struct PackVersionData {
    metadata: PackMetadata,
    /// In `pack.toml` order.
    args: Vec<PackArg>,
    config: Option<PackConfig>,
    /// `setup.sh`: POSIX sh, run after `lib.sh` (see
    /// [`install::compose`]). Idempotent; reads its args from the
    /// environment only.
    setup: Option<&'static str>,
}

/// The config of a pack version.
enum PackConfig {
    /// `config.{toml,json,yaml,yml}`: the same values for every entry.
    Static(Value),
    /// The source of `config.lua`: it makes the values of an entry from
    /// its args (see [`lua_config`]).
    Lua(&'static str),
}

impl Pack {
    pub fn metadata(&self) -> &PackMetadata {
        &self.0.metadata
    }

    /// The args of the version, in `pack.toml` order.
    pub fn args(&self) -> &[PackArg] {
        &self.0.args
    }

    /// The version with the arg values `args` and the defaults of the
    /// others. Each value must be a value of its arg (see
    /// [`ArgKind::parse`]): the config checks them
    /// ([`crate::config::pack_entries`]).
    pub fn configure(&self, args: &BTreeMap<String, ArgValue>) -> ConfiguredPack {
        debug_assert!(
            args.iter().all(|(key, value)| self
                .args()
                .iter()
                .any(|arg| arg.key == *key && arg.kind.check(value).is_ok())),
            "the arg values of {} fit its args",
            self.metadata().name
        );
        let filled = self
            .args()
            .iter()
            .map(|arg| {
                let value = args.get(&arg.key).unwrap_or(&arg.default);
                (arg.key.clone(), value.clone())
            })
            .collect();
        ConfiguredPack {
            pack: self.clone(),
            args: filled,
        }
    }

    /// The keys of the args, for error messages.
    pub fn known_args(&self) -> String {
        if self.args().is_empty() {
            return "it has no args".to_string();
        }
        let keys: Vec<&str> = self.args().iter().map(|arg| arg.key.as_str()).collect();
        format!("known: {}", keys.join(", "))
    }
}

/// One arg of a pack version: a value that a `[packs]` entry sets in
/// `args = { <key> = <value> }`.
#[derive(Clone, Debug)]
pub struct PackArg {
    /// `[a-z][a-z0-9-]*`, not `version`, `enabled` or `args`.
    pub key: String,
    /// What the arg does; the setup wizard shows it as the label of the
    /// arg's row.
    pub description: String,
    pub kind: ArgKind,
    /// The value when no config file sets the arg.
    pub default: ArgValue,
}

/// The kind of an arg value.
#[derive(Clone, Debug)]
pub enum ArgKind {
    /// `true` or `false`.
    Bool,
    /// One of `values`; with `other`, any non-empty string.
    Choice { values: Vec<String>, other: bool },
}

impl ArgKind {
    /// The arg value of the config value `raw`; the error text follows
    /// the arg's path.
    pub fn parse(&self, raw: &Value) -> Result<ArgValue, String> {
        let value = match raw {
            Value::Bool(b) => ArgValue::Bool(*b),
            Value::String(s) => ArgValue::Text(s.clone()),
            _ => return Err(self.wrong_type()),
        };
        self.check(&value)?;
        Ok(value)
    }

    /// Check that `value` is a value of this kind.
    fn check(&self, value: &ArgValue) -> Result<(), String> {
        match (self, value) {
            (ArgKind::Bool, ArgValue::Bool(_)) => Ok(()),
            (ArgKind::Choice { values, other }, ArgValue::Text(text)) => {
                if values.contains(text) || (*other && !text.is_empty()) {
                    Ok(())
                } else {
                    Err(format!(
                        "`{text}` is not valid ({})",
                        expected_choice(values, *other)
                    ))
                }
            }
            _ => Err(self.wrong_type()),
        }
    }

    /// The error for a value of another type.
    fn wrong_type(&self) -> String {
        match self {
            ArgKind::Bool => "must be true or false".to_string(),
            ArgKind::Choice { values, other } => {
                format!("must be a string ({})", expected_choice(values, *other))
            }
        }
    }
}

/// What the value of a choice arg must be, for error messages.
fn expected_choice(values: &[String], other: bool) -> String {
    if other {
        format!(
            "one of: {}, or any other non-empty string",
            values.join(", ")
        )
    } else {
        format!("one of: {}", values.join(", "))
    }
}

/// A checked arg value. Serializes as the config value (a boolean or a
/// string).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ArgValue {
    /// The value of an [`ArgKind::Bool`] arg.
    Bool(bool),
    /// The value of an [`ArgKind::Choice`] arg.
    Text(String),
}

impl fmt::Display for ArgValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgValue::Bool(b) => write!(f, "{b}"),
            ArgValue::Text(s) => f.write_str(s),
        }
    }
}

/// A pack version with its arg values, defaults filled (see
/// [`Pack::configure`]; the config checks the values).
/// Cheap to clone.
#[derive(Clone)]
pub struct ConfiguredPack {
    pack: Pack,
    /// One value per arg of the version (defaults filled), by key.
    args: BTreeMap<String, ArgValue>,
}

impl ConfiguredPack {
    pub fn metadata(&self) -> &PackMetadata {
        self.pack.metadata()
    }

    /// The value of every arg (defaults filled), by key.
    pub fn args(&self) -> &BTreeMap<String, ArgValue> {
        &self.args
    }

    /// The args whose value is not the version's default, by key.
    pub fn non_default_args(&self) -> Vec<(&str, &ArgValue)> {
        self.args
            .iter()
            .filter(|(key, value)| {
                self.pack
                    .args()
                    .iter()
                    .any(|arg| arg.key == **key && arg.default != **value)
            })
            .map(|(key, value)| (key.as_str(), value))
            .collect()
    }

    /// The config that the pack applies: its static document, or what
    /// its `config.lua` makes of the args (see [`lua_config::evaluate`]);
    /// an empty object without a config. Before a `config.lua` runs, the
    /// pack's directory ([`crate::cache::pack_mounts_dir`]) is created. A
    /// failing `config.lua` or directory is an error `pack <name>: …`.
    pub fn config_values(&self) -> anyhow::Result<Value> {
        match &self.pack.0.config {
            None => Ok(Value::Object(Map::new())),
            Some(PackConfig::Static(value)) => Ok(value.clone()),
            Some(PackConfig::Lua(source)) => {
                let metadata = self.metadata();
                let directory = crate::cache::pack_mounts_dir(&metadata.name)
                    .map_err(|e| anyhow::anyhow!("pack {}: {e:#}", metadata.name))?;
                lua_config::evaluate(metadata, source, &self.args, &directory)
            }
        }
    }

    /// The install of the version, if it has a setup script.
    pub fn setup_installer(&self) -> Option<InstallerScript> {
        let setup = self.pack.0.setup?;
        let metadata = self.metadata();
        let mut env = vec![
            ("AIRLOCK_PACK_API".to_string(), "1".to_string()),
            ("AIRLOCK_PACK_ID".to_string(), metadata.name.clone()),
        ];
        env.extend(self.args.iter().map(|(key, value)| {
            let name = key.to_ascii_uppercase().replace('-', "_");
            (format!("AIRLOCK_PACK_ARG_{name}"), value.to_string())
        }));

        // The definition of the pack: name, version and arg values (in
        // key order). The scripts do not count: a change of them ships as
        // a new version.
        let input = (&metadata.name, &metadata.version, &self.args);
        let json = serde_json::to_vec(&input).expect("fingerprint input serializes");
        let fingerprint = hex::encode(Sha256::digest(json));

        Some(InstallerScript {
            pack: metadata.name.clone(),
            label: metadata.label.clone(),
            script: install::compose::script(setup),
            env,
            fingerprint,
        })
    }
}

/// The install of a configured pack.
#[derive(Clone, Debug)]
pub struct InstallerScript {
    /// The pack name: the key in `installs.json`.
    pub pack: String,
    pub label: String,
    /// The whole script: wrapper, `lib.sh`, the pack's `setup.sh` (see
    /// [`install::compose::script`]).
    pub script: String,
    /// `AIRLOCK_PACK_API`, `AIRLOCK_PACK_ID`, and per arg
    /// `AIRLOCK_PACK_ARG_<KEY>` (key in upper case, `-` as `_`; a bool
    /// is `true` or `false`; args are never secrets).
    pub env: Vec<(String, String)>,
    /// Identifies the definition of the pack (name, version, args): a
    /// change needs a new sandbox.
    pub fingerprint: String,
}

#[cfg(test)]
mod tests;
