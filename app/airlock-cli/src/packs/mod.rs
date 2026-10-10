//! Packs.
//!
//! A pack is a named, versioned bundle of config that a project enables in
//! its `[packs]` table. Pack args let the user adjust a pack, for example
//! to select a tool version. A pack can also install software into the
//! sandbox with a setup script.
//!
//! This module loads the known packs and combines a pack with the user's
//! arg values into the config and the install script that the pack gives.
//! Pack authors can compute config with a script. Install scripts get
//! their args as environment variables and report their progress with a
//! status line protocol.

mod builtin;
pub mod install;
pub mod lua_config;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Load the known packs. At the moment these are only the built-in packs.
pub fn init() -> anyhow::Result<PackManager> {
    builtin::load(&builtin::BUILTIN_PACKS)
}

/// Load the known packs and the test pack `sample@1`, see [`init`].
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

/// The set of known packs. Cheap to clone.
#[derive(Clone)]
pub struct PackManager {
    /// All versions of all packs. Sorted by kind (see [`PackKind`]), then
    /// by name. The versions of one pack are sorted oldest first.
    packs: Arc<[Pack]>,
}

impl PackManager {
    /// Get the newest version of each built-in pack.
    /// Returns:
    ///   Packs sorted by kind, then by name. Documents, installs and
    ///   listings use this order.
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

    /// Find a pack version.
    /// Args:
    ///  - `name`: Pack name
    ///  - `version`: Pack version, for example `"1"`
    ///
    /// Returns:
    ///   The pack version, or `None` if it does not exist.
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

/// One version of a pack (`name@version`) with all its data.
/// Cheap to clone.
#[derive(Clone)]
pub struct Pack(Arc<PackVersionData>);

/// Name and description of a pack version.
pub struct PackMetadata {
    /// Key in the `[packs]` table.
    pub name: String,
    /// `"1"`, `"2"`, …
    pub version: String,
    /// Human-readable name.
    pub label: String,
    /// One-line description of the version. The setup wizard shows it.
    pub description: String,
    /// What the pack is for.
    pub kind: PackKind,
    /// Whether the version has a setup script (`setup.sh`).
    pub has_setup: bool,
}

/// What a pack is for. Packs are sorted by kind in the order of the variants.
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
    /// Args in `pack.toml` order.
    args: Vec<PackArg>,
    /// Config that the pack applies, if any.
    config: Option<PackConfig>,
    /// `setup.sh`: POSIX sh script that runs after `lib.sh` (see
    /// [`install::compose`]). It is idempotent. It reads its args only from
    /// the environment.
    setup: Option<&'static str>,
}

/// The config of a pack version.
enum PackConfig {
    /// `config.{toml,json,yaml,yml}`: the same values for every entry.
    Static(Value),
    /// Source of `config.lua`. The script makes the config values of an
    /// entry from its args (see [`lua_config`]).
    Lua(&'static str),
}

impl Pack {
    /// Name and description of the version.
    pub fn metadata(&self) -> &PackMetadata {
        &self.0.metadata
    }

    /// Args of the version, in `pack.toml` order.
    pub fn args(&self) -> &[PackArg] {
        &self.0.args
    }

    /// Apply arg values to the version.
    /// Args:
    ///  - `args`: Arg values by key. Each value must be valid for its arg
    ///    (see [`ArgKind::parse`]). [`crate::config::pack_entries`] checks
    ///    the values before this call.
    ///
    /// Returns:
    ///   The configured pack. Args not in `args` get their default value.
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

    /// List the arg keys as text for error messages.
    pub fn known_args(&self) -> String {
        if self.args().is_empty() {
            return "it has no args".to_string();
        }
        let keys: Vec<&str> = self.args().iter().map(|arg| arg.key.as_str()).collect();
        format!("known: {}", keys.join(", "))
    }
}

/// One arg of a pack version. A `[packs]` entry sets its value in
/// `args = { <key> = <value> }`.
#[derive(Clone, Debug)]
pub struct PackArg {
    /// `[a-z][a-z0-9-]*`, not `version`, `enabled` or `args`.
    pub key: String,
    /// What the arg does. The setup wizard shows it as the label of the
    /// arg row.
    pub description: String,
    /// Type of the arg value.
    pub kind: ArgKind,
    /// Value to use when no config file sets the arg.
    pub default: ArgValue,
}

/// The kind of an arg value.
#[derive(Clone, Debug)]
pub enum ArgKind {
    /// `true` or `false`.
    Bool,
    /// One of `values`. If `other` is true, also any non-empty string.
    Choice { values: Vec<String>, other: bool },
}

impl ArgKind {
    /// Parse and check an arg value.
    /// Args:
    ///  - `raw`: Value from the config file
    ///
    /// Returns:
    ///   The arg value, or an error text. The caller writes the error text
    ///   after the arg path.
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

    /// Error text for a value of the wrong type.
    fn wrong_type(&self) -> String {
        match self {
            ArgKind::Bool => "must be true or false".to_string(),
            ArgKind::Choice { values, other } => {
                format!("must be a string ({})", expected_choice(values, *other))
            }
        }
    }
}

/// Describe the valid values of a choice arg, for error messages.
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

/// A pack version with its arg values, made by [`Pack::configure`].
/// All args have a value. Cheap to clone.
#[derive(Clone)]
pub struct ConfiguredPack {
    pack: Pack,
    /// One value for each arg of the version (defaults included), by key.
    args: BTreeMap<String, ArgValue>,
}

impl ConfiguredPack {
    /// Name and description of the version.
    pub fn metadata(&self) -> &PackMetadata {
        self.pack.metadata()
    }

    /// Values of all args (defaults included), by key.
    pub fn args(&self) -> &BTreeMap<String, ArgValue> {
        &self.args
    }

    /// Get the args whose value is not the default, sorted by key. The
    /// install fingerprint uses them (see [`Self::setup_installer`]), so a
    /// new arg with a default does not change it.
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

    /// Get the config values that the pack applies.
    /// Args:
    ///  - `data_dir`: Airlock data directory, for the mount directory of the
    ///    pack (see [`crate::cache::pack_mounts_dir`])
    ///
    /// Returns:
    ///   The static config document, or the result of `config.lua` for the
    ///   arg values (see [`lua_config::evaluate`]). An empty object if the
    ///   pack has no config. An error `pack <name>: …` if `config.lua` or
    ///   the pack directory fails.
    pub fn config_values(&self, data_dir: &Path) -> anyhow::Result<Value> {
        match &self.pack.0.config {
            None => Ok(Value::Object(Map::new())),
            Some(PackConfig::Static(value)) => Ok(value.clone()),
            Some(PackConfig::Lua(source)) => {
                let metadata = self.metadata();
                // Create the pack directory before `config.lua` runs.
                let directory = crate::cache::pack_mounts_dir(data_dir, &metadata.name)
                    .map_err(|e| anyhow::anyhow!("pack {}: {e:#}", metadata.name))?;
                lua_config::evaluate(metadata, source, &self.args, &directory)
            }
        }
    }

    /// Make the installer of the version.
    /// Returns:
    ///   The installer, or `None` if the version has no setup script.
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

        // The fingerprint covers the name, the version and the arg values that
        // differ from their defaults (in key order). It does not cover the
        // scripts. So a breaking change must ship as a new version. A breaking
        // change changes the result of an existing set of arg values. For
        // example, a new boolean arg whose default (false) keeps the old result
        // is not breaking. A change of a default value is breaking.
        let args: BTreeMap<&str, &ArgValue> = self.non_default_args().into_iter().collect();
        let input = (&metadata.name, &metadata.version, args);
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

/// Installer of a configured pack.
#[derive(Clone, Debug)]
pub struct InstallerScript {
    /// Pack name. It is also the key in the install records.
    pub pack: String,
    /// Human-readable pack name.
    pub label: String,
    /// Full script: wrapper, `lib.sh` and the `setup.sh` of the pack (see
    /// [`install::compose::script`]).
    pub script: String,
    /// Environment variables for the script: `AIRLOCK_PACK_API`,
    /// `AIRLOCK_PACK_ID`, and `AIRLOCK_PACK_ARG_<KEY>` for each arg. The key
    /// is in upper case with `-` changed to `_`. A bool is `true` or
    /// `false`. Args are never secrets.
    pub env: Vec<(String, String)>,
    /// Identifies the pack definition (name, version, args that differ from
    /// their defaults). If it changes, a new sandbox is necessary.
    pub fingerprint: String,
}

#[cfg(test)]
mod tests;
