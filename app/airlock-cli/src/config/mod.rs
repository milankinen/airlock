//! Layered airlock configuration with pack support.
//!
//! Reads the user and project config files and merges them into one
//! validated config. The config of the enabled packs merges in with the
//! files. The rest of the program uses only the resolved config.
//!
//! Also handles the `[packs]` tables, the legacy `presets` lists and the
//! new config file that the setup wizard creates.

pub(crate) mod config_values;
pub(crate) mod de;
pub(crate) mod files;
pub(crate) mod generated;
pub(crate) mod legacy_presets;
pub(crate) mod merge;
pub(crate) mod pack_entries;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::config::config_values::{ConfigValues, Policy};
use crate::config::generated::GeneratedConfig;
use crate::config::merge::{merge_json, normalize_env, pack_conflicts};
use crate::packs::{ConfiguredPack, Pack, PackManager};

/// Load the config files of the project `project_root`. The user files come
/// from the home directory of the user, and the local project file from
/// `local_dir` (see [`crate::sandboxes::local_config_dir`]).
pub fn load(project_root: &Path, local_dir: &Path) -> anyhow::Result<LayeredConfig> {
    let home = dirs::home_dir().unwrap_or_default();
    LayeredConfig::load_from(&home, project_root, local_dir)
}

/// One config file (or in-memory document) before merging.
#[derive(Clone)]
struct Layer {
    /// Source of the layer (a file path), for logs and errors.
    origin: String,
    /// Config values of the file. The env is already normalized (see
    /// [`normalize_env`]), thus layers merge field by field no matter where
    /// they came from. A `presets` list (the released list form) is removed
    /// from the value. A `[packs]` table stays.
    value: serde_json::Value,
    /// Names in the `presets` list of the file, if it has one.
    legacy_presets: Option<Vec<String>>,
}

impl Layer {
    /// Make a layer.
    /// Args:
    ///  - `origin`: Source of the layer, for logs and errors
    ///  - `value`: Config values of the file
    ///
    /// Returns:
    ///   The layer, or an error if `presets` is not a list of names (see
    ///   [`legacy_presets::take_presets_key`]). [`LayeredConfig::resolve`]
    ///   checks the names later.
    fn new(origin: impl Into<String>, mut value: serde_json::Value) -> anyhow::Result<Self> {
        let origin = origin.into();
        normalize_env(&mut value);
        let legacy_presets = legacy_presets::take_presets_key(&mut value, &origin)?;
        Ok(Self {
            origin,
            value,
            legacy_presets,
        })
    }

    /// Make the layer of a discovered config file and log it.
    fn from_file((path, value): (PathBuf, serde_json::Value)) -> anyhow::Result<Self> {
        let layer = Self::new(path.display().to_string(), value)?;
        tracing::debug!("config: loaded {}", layer.origin);
        tracing::trace!("config: {}: {}", layer.origin, layer.value);
        Ok(layer)
    }

    /// Make the layer of a discovered user file (see [`Self::from_file`]).
    /// A `packs` key is an error, because only project-level files can
    /// enable packs.
    fn from_user_file(file: (PathBuf, serde_json::Value)) -> anyhow::Result<Self> {
        let layer = Self::from_file(file)?;
        anyhow::ensure!(
            layer.value.get("packs").is_none(),
            "`[packs]` is allowed only in project config files (airlock.toml or the local \
             project config); remove it from {}",
            layer.origin
        );
        Ok(layer)
    }
}

/// Command line overrides for [`LayeredConfig::resolve`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigOverrides {
    /// `airlock start --network <POLICY>`.
    pub network_policy: Option<Policy>,
}

/// The config files of a project, lowest precedence first:
/// user files < local project file < project files.
#[derive(Clone)]
pub struct LayeredConfig {
    /// `~/.airlock/airlock.<ext>`, `~/.airlock/config.<ext>`,
    /// `~/.airlock.<ext>`. They have no `[packs]` (see
    /// [`Layer::from_user_file`]).
    user: Vec<Layer>,
    /// `<local_dir>/airlock.<ext>` (see [`files::discover_in`])
    local: Option<Layer>,
    /// `<project_root>/airlock.<ext>`, `<project_root>/airlock.local.<ext>`
    project: Vec<Layer>,
    /// Project config that the setup wizard generated (see
    /// [`Self::with_generated_project`]). It is not written to disk yet.
    generated: Option<GeneratedConfig>,
}

impl LayeredConfig {
    /// True if the project has its own config: a project file or the local
    /// project file. User files alone do not count.
    pub fn has_project_config(&self) -> bool {
        !self.project.is_empty() || self.local.is_some()
    }

    /// Use a generated config as the project config.
    /// Args:
    ///  - `generated`: Config that the setup wizard made. It is not written
    ///    to disk yet.
    ///
    /// Returns:
    ///   The config, or an error if the project already has a config (see
    ///   [`Self::has_project_config`]) or `generated` is not valid TOML.
    pub fn with_generated_project(mut self, generated: GeneratedConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !self.has_project_config(),
            "the project has a config file already"
        );
        let origin = format!("setup wizard ({})", generated.path.display());
        let value: serde_json::Value =
            toml::from_str(&generated.toml).map_err(|e| anyhow::anyhow!("{origin}: {e}"))?;
        self.project = vec![Layer::new(origin, value)?];
        self.generated = Some(generated);
        Ok(self)
    }

    /// Get the image that the user files set in `vm.image`, if any. The
    /// highest user file with an image wins.
    pub fn user_image(&self) -> Option<UserImage> {
        self.user.iter().rev().find_map(|layer| {
            // `image = "<ref>"`, or `[vm.image]` with its `name`.
            let value = layer.value.pointer("/vm/image")?;
            let name = value.as_str().or_else(|| value.get("name")?.as_str())?;
            Some(UserImage {
                name: name.to_string(),
                value: value.clone(),
            })
        })
    }

    /// Get the project config that [`Self::with_generated_project`] added,
    /// if any. It is not saved yet (see
    /// [`crate::start::wizard::save_config`]).
    pub fn generated_project(&self) -> Option<&GeneratedConfig> {
        self.generated.as_ref()
    }

    /// Merge all layers and their packs into the final config.
    ///
    /// The merge order is:
    ///  1. The documents of the `presets` lists.
    ///  2. For each layer, lowest precedence first: the config values of the
    ///     packs whose highest entry is in that layer, then the values of
    ///     the layer.
    ///
    /// Thus a pack overrides the layers below its highest entry (a project
    /// pack overrides the user files). The layer of that entry overrides
    /// the pack.
    /// Args:
    ///  - `packs`: Known packs
    ///  - `overrides`: Command line overrides
    ///
    /// Returns:
    ///   The validated config and the enabled packs. An error lists all
    ///   problems, for example an unknown preset name, an invalid
    ///   `[packs]` entry, a failed `config.lua`, or two packs that set a
    ///   value differently.
    pub async fn resolve(
        &self,
        packs: &PackManager,
        overrides: &ConfigOverrides,
    ) -> anyhow::Result<ResolvedConfig> {
        let known = packs.builtin();
        // The names of the `presets` lists must be released names (see
        // `legacy_presets::validate_names`).
        let legacy_base = self.legacy_base(&known)?;
        let mut problems = Vec::new();
        let mut entries = Vec::new();
        let mut plain_layers = Vec::new();
        // Read the `[packs]` table of each layer separately. The entries
        // merge for each pack (see `pack_entries`).
        for layer in self.layers() {
            tracing::trace!("config: reading {}", layer.origin);
            let mut value = layer.value.clone();
            let layer_entries =
                pack_entries::read_layer_packs(&layer.origin, &mut value, &known, &mut problems);
            let pack_names = layer_entries.iter().map(|e| e.name.clone()).collect();
            entries.extend(layer_entries);
            plain_layers.push(PlainLayer { value, pack_names });
        }
        let configured = pack_entries::configure_packs(entries, packs, &known, &mut problems).await;
        if !problems.is_empty() {
            anyhow::bail!("invalid configuration\n{}", problems.join("\n"));
        }
        // A `config.lua` runs here, on each resolve. The values of the
        // enabled packs must not conflict across all layers.
        let pack_values = pack_configs(&configured).map_err(|problems| {
            anyhow::anyhow!("invalid configuration\n{}", problems.join("\n"))
        })?;
        let mut values = merge_config(legacy_base, plain_layers, &configured, pack_values)?;
        if let Some(policy) = overrides.network_policy {
            tracing::info!("network policy overridden by --network: {}", policy.label());
            values.network.policy = policy;
        }
        Ok(ResolvedConfig {
            values,
            packs: configured,
        })
    }

    /// Iterate the layers, lowest precedence first.
    fn layers(&self) -> impl Iterator<Item = &Layer> {
        self.user.iter().chain(&self.local).chain(&self.project)
    }

    /// Merge the documents of the `presets` lists of all layers (see
    /// [`legacy_presets::expand`]). The result applies below all layers and
    /// their packs.
    /// Args:
    ///  - `known`: Built-in packs, for the hint of an unknown name
    ///
    /// Returns:
    ///   The merged documents, or an error for a name that is not a
    ///   released one.
    fn legacy_base(&self, known: &[Pack]) -> anyhow::Result<serde_json::Value> {
        for layer in self.layers() {
            if let Some(names) = &layer.legacy_presets {
                legacy_presets::validate_names(&layer.origin, names, known)?;
            }
        }
        let names = self
            .layers()
            .filter_map(|layer| layer.legacy_presets.as_ref())
            .flatten()
            .map(String::as_str);
        legacy_presets::expand(names)
    }

    /// Make the config from its layers, each lowest precedence first. There
    /// is no generated project config yet.
    fn new(user: Vec<Layer>, local: Option<Layer>, project: Vec<Layer>) -> Self {
        Self {
            user,
            local,
            project,
            generated: None,
        }
    }

    /// Load the config files of `project_root`, with `home` as the home
    /// directory and the local project file in `local_dir` (see
    /// [`files::discover_in`]).
    pub(crate) fn load_from(
        home: &Path,
        project_root: &Path,
        local_dir: &Path,
    ) -> anyhow::Result<Self> {
        let files = files::discover_in(home, project_root, local_dir)?;
        Ok(Self::new(
            files
                .user
                .into_iter()
                .map(Layer::from_user_file)
                .collect::<anyhow::Result<_>>()?,
            files.local.map(Layer::from_file).transpose()?,
            files
                .project
                .into_iter()
                .map(Layer::from_file)
                .collect::<anyhow::Result<_>>()?,
        ))
    }

    /// The config of in-memory files, each `(origin, value)`, as if they
    /// were loaded from the user, local and project slots.
    #[cfg(test)]
    pub(crate) fn from_values(
        user: Vec<(&str, serde_json::Value)>,
        local: Option<(&str, serde_json::Value)>,
        project: Vec<(&str, serde_json::Value)>,
    ) -> anyhow::Result<Self> {
        let layers = |files: Vec<(&str, serde_json::Value)>| {
            files
                .into_iter()
                .map(|(origin, value)| Layer::new(origin, value))
                .collect::<anyhow::Result<Vec<_>>>()
        };
        Ok(Self::new(
            layers(user)?,
            local
                .map(|(origin, value)| Layer::new(origin, value))
                .transpose()?,
            layers(project)?,
        ))
    }
}

/// Image that the user files set (see [`LayeredConfig::user_image`]).
#[derive(Clone)]
pub struct UserImage {
    /// Image reference (`image`, or the `name` of `[vm.image]`).
    pub name: String,
    /// `vm.image` as it is in the file.
    pub value: serde_json::Value,
}

/// A resolved config and the enabled packs.
pub struct ResolvedConfig {
    /// Final config values, with all overrides (also the `--network`
    /// policy, if set).
    pub values: ConfigValues,
    /// All enabled `[packs]` entries, in pack order.
    pub packs: Vec<ConfiguredPack>,
}

impl ResolvedConfig {
    /// Make the config of the install boot (see
    /// [`crate::packs::install::phase::install_config`]).
    pub(crate) fn install_config(&self) -> anyhow::Result<ConfigValues> {
        crate::packs::install::phase::install_config(self.values.clone())
    }
}

/// Get the config values of the packs.
/// Args:
///  - `packs`: Configured packs
///
/// Returns:
///   The config values in pack order, with normalized env (see
///   [`normalize_env`]). Or the problems, one line each: a pack whose
///   `config.lua` fails, or a value that two packs set differently (see
///   [`pack_conflicts`]).
fn pack_configs(packs: &[ConfiguredPack]) -> Result<Vec<serde_json::Value>, Vec<String>> {
    let mut docs = Vec::new();
    let mut problems = Vec::new();
    for pack in packs {
        let metadata = pack.metadata();
        tracing::debug!(
            "config: applying pack `{}` version {}",
            metadata.name,
            metadata.version
        );
        match pack.config_values() {
            Ok(mut value) => {
                normalize_env(&mut value);
                docs.push((metadata.name.clone(), value));
            }
            Err(e) => problems.push(format!("* {e:#}")),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let conflicts = pack_conflicts(&docs);
    if !conflicts.is_empty() {
        return Err(conflicts
            .into_iter()
            .map(|conflict| format!("* {conflict}"))
            .collect());
    }
    Ok(docs.into_iter().map(|(_, value)| value).collect())
}

/// One layer as input to [`merge_config`].
struct PlainLayer {
    /// Values of the layer, without `packs` and `presets`.
    value: serde_json::Value,
    /// Names of the packs that the `[packs]` table of the layer has entries
    /// for.
    pack_names: Vec<String>,
}

/// Merge the presets, layers and packs, then parse and validate the result.
/// Args:
///  - `legacy_base`: Merged documents of the `presets` lists. It applies
///    first.
///  - `layers`: Layers, lowest precedence first. For each layer, the config
///    values of the packs whose highest entry is in that layer apply first,
///    then the values of the layer.
///  - `packs`: Configured packs
///  - `pack_values`: Config values of `packs`, in the same order
///
/// Returns:
///   The validated config values.
fn merge_config(
    legacy_base: serde_json::Value,
    layers: Vec<PlainLayer>,
    packs: &[ConfiguredPack],
    pack_values: Vec<serde_json::Value>,
) -> anyhow::Result<ConfigValues> {
    // Each configured pack has an entry, thus it has a layer.
    let mut layer_packs = vec![Vec::new(); layers.len()];
    for (pack, value) in packs.iter().zip(pack_values) {
        let name = &pack.metadata().name;
        if let Some(top) = layers.iter().rposition(|l| l.pack_names.contains(name)) {
            layer_packs[top].push(value);
        }
    }
    let mut merged = legacy_base;
    for (layer, pack_values) in layers.into_iter().zip(layer_packs) {
        for value in pack_values {
            merged = merge_json(merged, value);
        }
        merged = merge_json(merged, layer.value);
    }
    tracing::trace!("config: merged result: {merged}");
    config_values::parse(merged)
}
