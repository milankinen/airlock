//! Packs.
//!
//! A pack is a named, versioned bundle of config that a project enables in
//! its `[packs]` table. Pack args let the user adjust a pack, for example
//! to select a tool version. A pack can also install software into the
//! sandbox with a setup script.
//!
//! Packs come from three sources: the packs that airlock has built in, the
//! user packs in the user pack directories of the settings, and the
//! project packs in directories of the project.
//!
//! This module loads the known packs and combines a pack with the user's
//! arg values into the config and the install script that the pack gives.
//! Pack authors can compute config with a script. Install scripts get
//! their args as environment variables and report their progress with a
//! status line protocol.

pub mod approval;
mod builtin;
mod disk;
pub mod id;
pub mod install;
mod loader;
pub mod lua_config;
pub mod setup_env;
pub mod snapshot;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
pub use setup_env::{EnvMacKey, SetupEnv};
use sha2::{Digest, Sha256};

use crate::data_dir::DataDir;
pub use crate::packs::id::{PackId, PackRef};
use crate::settings::PackSettings;
use crate::util::VerifiedPath;

/// Load the known packs: the built-in packs and the user packs. Prints a
/// warning for each directory of the `packs.dirs` setting that does not
/// exist.
/// Args:
///  - `settings`: The `[packs]` settings
///  - `home`: Home directory of the user, for `~` in the settings
///
/// Returns:
///   The packs, or an error if a pack is not valid or a user pack
///   directory cannot be read.
pub fn init(settings: &PackSettings, home: &VerifiedPath) -> anyhow::Result<PackManager> {
    let dirs = settings.user_dirs(home)?;
    let (manager, warnings) = builtin::load(&builtin::BUILTIN_PACKS)?
        .with_user_packs(&dirs, settings.defaults.clone())?;
    for warning in warnings {
        crate::cli::log!("{} {warning}", crate::cli::yellow("!"));
    }
    Ok(manager)
}

/// Load only the built-in packs.
#[cfg(test)]
pub fn init_builtin() -> anyhow::Result<PackManager> {
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
    /// by name. The built-in packs come before the user packs of the same
    /// kind. The versions of one pack are sorted oldest first.
    packs: Arc<[Pack]>,
    /// Pack ids from the `packs.defaults` setting, as the user wrote them.
    defaults: Arc<[String]>,
}

impl PackManager {
    /// Make the set of packs from `packs`.
    /// Args:
    ///  - `packs`: Packs sorted as [`PackManager::packs`] tells
    fn new(packs: Arc<[Pack]>) -> Self {
        Self {
            packs,
            defaults: Arc::new([]),
        }
    }

    /// Add the user packs of `dirs` to the packs.
    /// Args:
    ///  - `dirs`: User pack directories. The flag is true if the user set the
    ///    directory. A missing directory that the user set gives a warning.
    ///  - `defaults`: Pack ids from the `packs.defaults` setting
    ///
    /// Returns:
    ///   The packs and the warnings, or an error if a user pack is not
    ///   valid, two directories have the same pack, or a directory cannot be
    ///   read.
    pub fn with_user_packs(
        self,
        dirs: &[(VerifiedPath, bool)],
        defaults: Vec<String>,
    ) -> anyhow::Result<(Self, Vec<String>)> {
        let (local, warnings) = disk::load_user_packs(dirs)?;
        let mut packs: Vec<Pack> = self.packs.iter().cloned().collect();
        packs.extend(local.into_iter().map(|data| Pack(Arc::new(data))));
        // The sort is stable. Thus the built-in packs stay before the user
        // packs of the same kind, and the names stay in order.
        packs.sort_by_key(|pack| pack.metadata().kind);
        let manager = Self {
            packs: packs.into(),
            defaults: defaults.into(),
        };
        Ok((manager, warnings))
    }

    /// Get the pack ids of the `packs.defaults` setting. The setup wizard
    /// selects these packs at the start.
    /// Returns:
    ///   The ids of the known packs, and the ids that no offered pack has.
    ///   Only the setup wizard (of `start` and `templates create`) warns
    ///   about the unknown ids. Other commands ignore them.
    pub fn defaults(&self) -> (Vec<PackId>, Vec<String>) {
        let offered = self.offered();
        let mut known = Vec::new();
        let mut unknown = Vec::new();
        for text in self.defaults.iter() {
            match PackId::parse(text).filter(|id| offered.iter().any(|p| p.metadata().id == *id)) {
                Some(id) => known.push(id),
                None => unknown.push(text.clone()),
            }
        }
        (known, unknown)
    }

    /// Get the packs that a config can name and that the setup wizard
    /// offers: the newest version of each pack.
    /// Returns:
    ///   Packs sorted by kind, then by name. Documents, installs and
    ///   listings use this order.
    pub fn offered(&self) -> Vec<Pack> {
        let mut newest: Vec<Pack> = Vec::new();
        for pack in self.packs.iter() {
            match newest.last_mut() {
                Some(last) if last.metadata().id == pack.metadata().id => {
                    *last = pack.clone();
                }
                _ => newest.push(pack.clone()),
            }
        }
        newest
    }

    /// Find a pack version.
    /// Args:
    ///  - `id`: Pack id
    ///  - `version`: Pack version, for example `"1"`. `None` for a pack
    ///    with no versions.
    ///
    /// Returns:
    ///   The pack version, or `None` if it does not exist.
    #[allow(
        clippy::unused_async,
        reason = "remote packs resolve over the network later"
    )]
    pub async fn resolve(&self, id: &PackId, version: Option<&str>) -> Option<Pack> {
        self.packs
            .iter()
            .find(|p| p.metadata().id == *id && p.metadata().version.as_deref() == version)
            .cloned()
    }
}

/// Load a project pack from the disk. The config resolve loads each project
/// pack that a config file names, on each resolve.
/// Args:
///  - `id`: Pack id. It must be a project pack id (`./path`).
///  - `project_root`: Project root directory. The pack path is relative to
///    it.
///
/// Returns:
///   The pack, or an error if the pack directory does not exist, its path has
///   a link, or the pack is not valid.
pub fn load_project_pack(id: &PackId, project_root: &VerifiedPath) -> anyhow::Result<Pack> {
    disk::load_project_pack(project_root, id.clone()).map(|data| Pack(Arc::new(data)))
}

/// One version of a pack (`id@version`) with all its data.
/// Cheap to clone.
#[derive(Clone)]
pub struct Pack(Arc<PackVersionData>);

/// Name and description of a pack version.
pub struct PackMetadata {
    /// Key in the `[packs]` table.
    pub id: PackId,
    /// `"1"`, `"2"`, … for a built-in pack. `None` for a pack with no
    /// versions.
    pub version: Option<String>,
    /// Human-readable name.
    pub label: String,
    /// One-line description of the version. The setup wizard shows it.
    pub description: String,
    /// What the pack is for.
    pub kind: PackKind,
    /// Whether the version has a setup script (`setup.sh`).
    pub has_setup: bool,
    /// Where the pack comes from.
    pub source: PackSource,
}

/// Where the files of a pack come from.
#[derive(Clone, Debug)]
pub enum PackSource {
    /// The binary of airlock.
    Builtin,
    /// A sub-directory of a user pack directory.
    Local {
        /// The pack directory.
        dir: VerifiedPath,
    },
    /// A directory in the project.
    Project {
        /// The pack directory.
        dir: VerifiedPath,
        /// Name of the pack directory in the pack mount directory:
        /// `project.<hash>`. The hash covers the project root and the pack
        /// path, so the same pack path in two projects gets two directories.
        dir_name: String,
    },
}

/// Make the text `<id> <version>` of a pack, or only `<id>` for a pack
/// with no version. Listings show packs with this text.
pub fn id_with_version(id: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{id} {version}"),
        None => id.to_string(),
    }
}

impl PackMetadata {
    /// Make the text `<id> <version>` of the pack (see [`id_with_version`]).
    pub fn id_with_version(&self) -> String {
        id_with_version(self.id.as_str(), self.version.as_deref())
    }

    /// Get the directory name of the pack in the pack mount directory (see
    /// [`crate::cache::pack_mounts_dir`]).
    /// Returns:
    ///   The name for a built-in pack, `local.<name>` for a user pack, and
    ///   `project.<hash>` for a project pack (see [`PackSource::Project`]).
    pub fn dir_name(&self) -> std::borrow::Cow<'_, str> {
        match &self.source {
            PackSource::Project { dir_name, .. } => std::borrow::Cow::Borrowed(dir_name),
            PackSource::Builtin | PackSource::Local { .. } => self.id.dir_name(),
        }
    }
}

/// What a pack is for. Packs are sorted by kind in the order of the variants.
/// The `kind` of `pack.toml` sets it. A pack with no kind is an "other" pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackKind {
    /// A Linux distribution for the sandbox.
    Distro,
    /// A coding agent.
    Agent,
    /// A tool or a language.
    Tool,
    /// A pack of a different kind.
    Other,
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
    setup: Option<Arc<str>>,
    /// SHA-256 of the pack files, for a pack from the disk. `None` for a
    /// built-in pack.
    content_hash: Option<[u8; 32]>,
    /// Snapshot of the pack files, for a pack from the disk. It has the same
    /// bytes as [`Self::content_hash`]. `None` for a built-in pack.
    snapshot: Option<snapshot::PackSnapshot>,
}

/// The config of a pack version.
enum PackConfig {
    /// `config.{toml,json,yaml,yml}`: the same values for every entry.
    Static(Value),
    /// Source of `config.lua`. The script makes the config values of an
    /// entry from its args (see [`lua_config`]).
    Lua(Arc<str>),
}

impl Pack {
    /// Name and description of the version.
    pub fn metadata(&self) -> &PackMetadata {
        &self.0.metadata
    }

    /// Get the SHA-256 of the pack files.
    /// Returns:
    ///   The hash for a pack from the disk, `None` for a built-in pack.
    pub fn content_hash(&self) -> Option<&[u8; 32]> {
        self.0.content_hash.as_ref()
    }

    /// Get the snapshot of the pack files.
    /// Returns:
    ///   The snapshot for a pack from the disk, `None` for a built-in pack.
    pub fn snapshot(&self) -> Option<&snapshot::PackSnapshot> {
        self.0.snapshot.as_ref()
    }

    /// Get the source of the `config.lua` of the pack, as it runs.
    /// Returns:
    ///   The source, or `None` if the pack has no `config.lua`.
    pub fn lua_config(&self) -> Option<&str> {
        match &self.0.config {
            Some(PackConfig::Lua(source)) => Some(source),
            Some(PackConfig::Static(_)) | None => None,
        }
    }

    /// Return true if `self` and `other` are the same loaded pack: the same
    /// files in memory.
    pub fn same_as(&self, other: &Pack) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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
            self.metadata().id
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
            setup_env: SetupEnv::default(),
            env_mac: None,
            env_key_macs: BTreeMap::new(),
        }
    }

    /// List the arg keys as text for error messages.
    pub fn known_args(&self) -> String {
        if self.args().is_empty() {
            return crate::messages::packs::ARGS_NONE.to_string();
        }
        let keys: Vec<&str> = self.args().iter().map(|arg| arg.key.as_str()).collect();
        crate::messages::packs::args_known(keys.join(", "))
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
                    Err(crate::messages::packs::arg_value_invalid(
                        text,
                        expected_choice(values, *other),
                    ))
                }
            }
            _ => Err(self.wrong_type()),
        }
    }

    /// Error text for a value of the wrong type.
    fn wrong_type(&self) -> String {
        match self {
            ArgKind::Bool => crate::messages::packs::ARG_MUST_BE_BOOL.to_string(),
            ArgKind::Choice { values, other } => {
                crate::messages::packs::arg_must_be_string(expected_choice(values, *other))
            }
        }
    }
}

/// Describe the valid values of a choice arg, for error messages.
fn expected_choice(values: &[String], other: bool) -> String {
    if other {
        crate::messages::packs::arg_expected_choice_or_other(values.join(", "))
    } else {
        crate::messages::packs::arg_expected_choice(values.join(", "))
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
/// All args have a value. The config resolve adds the setup env (see
/// [`Self::set_setup_env`]).
#[derive(Clone)]
pub struct ConfiguredPack {
    pack: Pack,
    /// One value for each arg of the version (defaults included), by key.
    args: BTreeMap<String, ArgValue>,
    /// Values that `config.lua` gives to the setup script. Empty until the
    /// config resolves.
    setup_env: SetupEnv,
    /// Keyed hash of [`Self::setup_env`] (see [`SetupEnv::mac`]). `None` if
    /// the setup env is empty.
    env_mac: Option<[u8; 32]>,
    /// Keyed hash of each setup env value, by key (see
    /// [`SetupEnv::key_macs`]). The install snapshot keeps them.
    env_key_macs: BTreeMap<String, [u8; 32]>,
}

impl ConfiguredPack {
    /// Name and description of the version.
    pub fn metadata(&self) -> &PackMetadata {
        self.pack.metadata()
    }

    /// The pack version.
    pub fn pack(&self) -> &Pack {
        &self.pack
    }

    /// Values of all args (defaults included), by key.
    pub fn args(&self) -> &BTreeMap<String, ArgValue> {
        &self.args
    }

    /// Values that `config.lua` gives to the setup script. Empty if the
    /// config did not resolve.
    pub fn setup_env(&self) -> &SetupEnv {
        &self.setup_env
    }

    /// Set the setup env that `config.lua` gave (see [`Self::config_values`]).
    /// Args:
    ///  - `setup_env`: The setup env
    ///  - `key`: Hash key for the install fingerprint. `None` is valid only
    ///    for an empty setup env.
    pub fn set_setup_env(&mut self, setup_env: SetupEnv, key: Option<&EnvMacKey>) {
        debug_assert!(setup_env.is_empty() || key.is_some(), "a hash key is set");
        (self.env_mac, self.env_key_macs) = match key {
            Some(key) if !setup_env.is_empty() => {
                (Some(setup_env.mac(key)), setup_env.key_macs(key))
            }
            _ => (None, BTreeMap::new()),
        };
        self.setup_env = setup_env;
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

    /// Get the config values that the pack applies. This is the only place
    /// where the `config.lua` of a pack runs.
    /// Args:
    ///  - `data_dir`: Airlock data directory, for the mount directory of the
    ///    pack (see [`crate::cache::pack_mounts_dir`])
    ///  - `approved`: Proof that the user approved this pack (see
    ///    [`approval`])
    ///
    /// Returns:
    ///   The static config document, or the result of `config.lua` for the
    ///   arg values (see [`lua_config::evaluate`]). An empty object if the
    ///   pack has no config. Also the setup env: only `config.lua` can set
    ///   it. An error `pack <name>: …` if `config.lua` or the pack directory
    ///   fails, or if `approved` is for another pack.
    pub fn config_values(
        &self,
        data_dir: &DataDir,
        approved: &approval::Approved,
    ) -> anyhow::Result<(Value, SetupEnv)> {
        // The token must be for these files in memory. A token of another
        // pack is a program error, but fail closed.
        anyhow::ensure!(
            approved.covers(&self.pack),
            crate::messages::packs::pack_not_approved(&self.metadata().id)
        );
        match &self.pack.0.config {
            None => Ok((Value::Object(Map::new()), SetupEnv::default())),
            Some(PackConfig::Static(value)) => Ok((value.clone(), SetupEnv::default())),
            Some(PackConfig::Lua(source)) => {
                let metadata = self.metadata();
                // Create the pack directory before `config.lua` runs.
                let directory = crate::cache::pack_mounts_dir(data_dir, &metadata.dir_name())
                    .map_err(|e| {
                        anyhow::anyhow!(crate::messages::packs::pack_failed(
                            &metadata.id,
                            format!("{e:#}")
                        ))
                    })?;
                lua_config::evaluate(metadata, source, &self.args, &directory)
            }
        }
    }

    /// Make the installer of the version.
    /// Returns:
    ///   The installer, or `None` if the version has no setup script.
    pub fn setup_installer(&self) -> Option<InstallerScript> {
        let setup = self.pack.0.setup.as_deref()?;
        let metadata = self.metadata();
        let mut env = vec![
            ("AIRLOCK_PACK_API".to_string(), "1".to_string()),
            ("AIRLOCK_PACK_ID".to_string(), metadata.id.to_string()),
        ];
        env.extend(self.args.iter().map(|(key, value)| {
            let name = key.to_ascii_uppercase().replace('-', "_");
            (format!("AIRLOCK_PACK_ARG_{name}"), value.to_string())
        }));
        env.extend(self.setup_env.vars());

        // The arg values that differ from their defaults (in key order). For
        // a built-in pack, the fingerprint does not cover the scripts. So a
        // breaking change must ship as a new version. A breaking change
        // changes the result of an existing set of arg values. For example, a
        // new boolean arg whose default (false) keeps the old result is not
        // breaking. A change of a default value is breaking.
        let args: BTreeMap<&str, &ArgValue> = self.non_default_args().into_iter().collect();
        // The setup env values are secrets. Only their keyed hash goes in.
        let env_mac = self.env_mac.map(hex::encode);
        let json = match (self.pack.content_hash(), env_mac) {
            // A built-in pack with no setup env. `Some(version)` serializes
            // as the plain version text. Thus the fingerprints of existing
            // sandboxes stay the same.
            (None, None) => serde_json::to_vec(&(metadata.id.as_str(), &metadata.version, args)),
            (None, Some(mac)) => {
                serde_json::to_vec(&(metadata.id.as_str(), &metadata.version, args, mac))
            }
            // A pack from the disk has no versions. Its files can change at
            // any time, so the hash of the files goes in.
            (Some(hash), None) => {
                serde_json::to_vec(&(metadata.id.as_str(), args, hex::encode(hash)))
            }
            (Some(hash), Some(mac)) => {
                serde_json::to_vec(&(metadata.id.as_str(), args, hex::encode(hash), mac))
            }
        }
        .expect("fingerprint input serializes");
        let fingerprint = hex::encode(Sha256::digest(json));

        // A built-in pack has its files in the binary. Its snapshot keeps
        // only the version, the args and the setup env hashes.
        let mut snapshot = self.pack.snapshot().cloned().unwrap_or_default();
        snapshot.id = metadata.id.to_string();
        snapshot.version.clone_from(&metadata.version);
        snapshot.args = self
            .args
            .iter()
            .map(|(key, value)| (key.clone(), value.to_string()))
            .collect();
        snapshot.env_keys = self
            .env_key_macs
            .iter()
            .map(|(key, mac)| (key.clone(), hex::encode(mac)))
            .collect();

        Some(InstallerScript {
            pack: metadata.id.to_string(),
            label: metadata.label.clone(),
            script: install::compose::script(setup),
            env,
            fingerprint,
            snapshot,
        })
    }
}

/// Installer of a configured pack. `Debug` hides the setup env values.
#[derive(Clone)]
pub struct InstallerScript {
    /// Pack id. It is also the key in the install records.
    pub pack: String,
    /// Human-readable pack name.
    pub label: String,
    /// Full script: wrapper, `lib.sh` and the `setup.sh` of the pack (see
    /// [`install::compose::script`]).
    pub script: String,
    /// Environment variables for the script: `AIRLOCK_PACK_API`,
    /// `AIRLOCK_PACK_ID`, `AIRLOCK_PACK_ARG_<KEY>` for each arg, and
    /// `AIRLOCK_PACK_ENV_<KEY>` for each setup env key. The arg key is in
    /// upper case with `-` changed to `_`. A bool is `true` or `false`.
    /// Args are never secrets, setup env values can be secrets.
    pub env: Vec<(String, String)>,
    /// Identifies the pack definition: the id, the args that differ from
    /// their defaults, the version of a built-in pack or the hash of the
    /// files of a pack from the disk, and the keyed hash of the setup env.
    /// If it changes, the pack must install again.
    pub fingerprint: String,
    /// Copy of the pack definition for the install snapshot (see
    /// [`install::snapshots`]). It has no setup env values.
    pub snapshot: snapshot::PackSnapshot,
}

impl fmt::Debug for InstallerScript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The setup env values can be secrets. Show only their names.
        let env: Vec<(&str, &str)> = self
            .env
            .iter()
            .map(|(key, value)| {
                if key.starts_with(setup_env::ENV_PREFIX) {
                    (key.as_str(), "<hidden>")
                } else {
                    (key.as_str(), value.as_str())
                }
            })
            .collect();
        f.debug_struct("InstallerScript")
            .field("pack", &self.pack)
            .field("label", &self.label)
            .field("script", &self.script)
            .field("env", &env)
            .field("fingerprint", &self.fingerprint)
            .field("snapshot", &self.snapshot.id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
