//! Helpers that load and resolve config files for tests, and a vault with
//! only a host env.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use airlock_test_utils::{TempDir, block_on, temp_dir};

use crate::config::config_values::ConfigValues;
use crate::config::{ConfigOverrides, LayeredConfig, ResolvedConfig};
use crate::packs::{ArgKind, ArgValue, ConfiguredPack, Pack};
use crate::vault::{DisabledStorage, Vault, VaultStorageType};

/// Resolve one project file `airlock.toml` with the content `toml`. Uses
/// the built-in packs and the test pack `sample@1` (see
/// [`crate::packs::init_with_sample`]).
pub fn resolve_project_toml(toml: &str) -> anyhow::Result<ResolvedConfig> {
    let layers =
        LayeredConfig::from_values(vec![], None, vec![("airlock.toml", toml::from_str(toml)?)])?;
    resolve_layers(&layers, ConfigOverrides::default())
}

/// The error text (`{:#}`) of [`resolve_project_toml`]. Panics if `toml`
/// resolves.
pub fn project_toml_error(toml: &str) -> String {
    match resolve_project_toml(toml) {
        Ok(_) => panic!("config resolves:\n{toml}"),
        Err(e) => format!("{e:#}"),
    }
}

/// Resolve `layers` with `overrides`. Uses the built-in packs and the test
/// pack `sample@1`.
pub fn resolve_layers(
    layers: &LayeredConfig,
    overrides: ConfigOverrides,
) -> anyhow::Result<ResolvedConfig> {
    block_on(layers.resolve(&crate::packs::init_with_sample(), &overrides))
}

/// A temporary home directory and project directory with real config
/// files. It loads and resolves them as the CLI does.
pub struct ConfigDirs {
    dir: TempDir,
}

impl ConfigDirs {
    /// Make empty home and project directories.
    pub fn new() -> Self {
        let dir = temp_dir();
        std::fs::create_dir(dir.path().join("home")).unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        Self { dir }
    }

    /// The home directory.
    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// The project directory.
    pub fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// Write `content` to `rel` under the home directory.
    pub fn user_file(&self, rel: &str, content: &str) -> &Self {
        write_file(&self.home().join(rel), content);
        self
    }

    /// Write `content` to `rel` under the project directory.
    pub fn project_file(&self, rel: &str, content: &str) -> &Self {
        write_file(&self.project().join(rel), content);
        self
    }

    /// The path of `rel` under the project directory, as error messages
    /// show it.
    pub fn project_origin(&self, rel: &str) -> String {
        self.project().join(rel).display().to_string()
    }

    /// Find and parse the config files.
    pub fn load(&self) -> anyhow::Result<LayeredConfig> {
        LayeredConfig::load_from(&self.home(), &self.project())
    }

    /// Load and resolve the config files (see [`resolve_layers`]).
    pub fn resolve(&self) -> anyhow::Result<ResolvedConfig> {
        resolve_layers(&self.load()?, ConfigOverrides::default())
    }

    /// The resolved config values. Panics on a config error.
    pub fn values(&self) -> ConfigValues {
        self.resolve().unwrap().values
    }

    /// The error text (`{:#}`) of [`Self::resolve`]. Panics if it resolves.
    pub fn error(&self) -> String {
        match self.resolve() {
            Ok(_) => panic!("config resolves"),
            Err(e) => format!("{e:#}"),
        }
    }
}

impl Default for ConfigDirs {
    fn default() -> Self {
        Self::new()
    }
}

/// Write `content` to `path` and create its parent directories.
fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A vault with no secret storage and with the host env `host_env`.
pub fn host_env_vault(host_env: &[(&str, &str)]) -> Vault {
    Vault::new_with(
        Box::new(DisabledStorage),
        host_env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
        VaultStorageType::Disabled,
    )
}

/// Configure `pack` with each combination of its arg values: both values
/// of a bool arg and each listed value of a choice arg.
pub fn configured_variants(pack: &Pack) -> Vec<ConfiguredPack> {
    let mut variants = vec![BTreeMap::<String, ArgValue>::new()];
    for arg in pack.args() {
        let choices: Vec<ArgValue> = match &arg.kind {
            ArgKind::Bool => vec![ArgValue::Bool(true), ArgValue::Bool(false)],
            ArgKind::Choice { values, .. } => values.iter().cloned().map(ArgValue::Text).collect(),
        };
        variants = variants
            .into_iter()
            .flat_map(|values| {
                let key = &arg.key;
                choices.iter().map(move |choice| {
                    let mut values = values.clone();
                    values.insert(key.clone(), choice.clone());
                    values
                })
            })
            .collect();
    }
    variants.iter().map(|args| pack.configure(args)).collect()
}
