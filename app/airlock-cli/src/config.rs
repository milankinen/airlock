//! Hierarchical TOML configuration with pack support.
//!
//! [`load`] reads up to six config files (three user files, the local
//! project file, the project files; see [`files::discover_in`]) into a
//! [`LayeredConfig`]. Only the project-level files (the local project file
//! and the project files) may enable packs: a `[packs]` table in a user
//! file is an error. [`LayeredConfig::resolve`] merges the files with
//! deep-merge semantics, each over the config values of the packs whose
//! highest entry it holds, and validates the result with `smart-config`.

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

/// Load the config files of the project in the current directory, with
/// the user's home directory for the user files.
pub fn load() -> anyhow::Result<LayeredConfig> {
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("cannot determine the current directory: {e}"))?;
    let project_root = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let home = dirs::home_dir().unwrap_or_default();
    LayeredConfig::load_from(&home, &project_root)
}

/// One config file (or in-memory document) before merging.
///
/// `value` is already env-normalized (see [`normalize_env`]), so layers
/// merge field-wise no matter where they came from. A `presets` list (the
/// released list form) is taken out of `value`; a `[packs]` table stays.
#[derive(Clone)]
struct Layer {
    /// Where the layer came from (a file path), for logs and errors.
    origin: String,
    value: serde_json::Value,
    /// The names of the file's `presets` list, if it has one.
    legacy_presets: Option<Vec<String>>,
}

impl Layer {
    /// The layer of `value` from `origin`. A `presets` value that is not
    /// a list of names is an error (see
    /// [`legacy_presets::take_presets_key`]); the names of a list are
    /// checked in [`LayeredConfig::resolve`].
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

    /// The layer of a discovered config file, logged as loaded.
    fn from_file((path, value): (PathBuf, serde_json::Value)) -> anyhow::Result<Self> {
        let layer = Self::new(path.display().to_string(), value)?;
        tracing::debug!("config: loaded {}", layer.origin);
        tracing::trace!("config: {}: {}", layer.origin, layer.value);
        Ok(layer)
    }

    /// The layer of a discovered user file (see [`Self::from_file`]). A
    /// `packs` key is an error: packs belong in the project-level files.
    fn from_user_file(file: (PathBuf, serde_json::Value)) -> anyhow::Result<Self> {
        let layer = Self::from_file(file)?;
        anyhow::ensure!(
            layer.value.get("packs").is_none(),
            "`[packs]` is allowed only in project config files (airlock.toml, \
             .airlock/airlock.toml); remove it from {}",
            layer.origin
        );
        Ok(layer)
    }
}

/// What `--network` overrides in [`LayeredConfig::resolve`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigOverrides {
    /// `airlock start --network <POLICY>`.
    pub network_policy: Option<Policy>,
}

/// The config files of a project, lowest precedence first:
/// user files < local project file < project files.
#[derive(Clone)]
pub struct LayeredConfig {
    /// `~/.airlock/airlock.<ext>`, `~/.airlock/config.<ext>`, `~/.airlock.<ext>`;
    /// without `[packs]` when loaded (see [`Layer::from_user_file`]).
    user: Vec<Layer>,
    /// `<project_root>/.airlock/airlock.<ext>`
    local: Option<Layer>,
    /// `<project_root>/airlock.<ext>`, `<project_root>/airlock.local.<ext>`
    project: Vec<Layer>,
    /// The project config the setup wizard generated (see
    /// [`Self::with_generated_project`]), not written to disk yet.
    generated: Option<GeneratedConfig>,
}

impl LayeredConfig {
    /// True if the project has its own config: a project file or the local
    /// project file. User files alone do not count.
    pub fn has_project_config(&self) -> bool {
        !self.project.is_empty() || self.local.is_some()
    }

    /// Add `generated` (the config the setup wizard produced, not written
    /// to disk yet) as the project config. Only for a project without
    /// config (see [`Self::has_project_config`]).
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

    /// The image that the user files set (`vm.image`), if any: the
    /// highest user file with one wins.
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

    /// The project config the setup wizard generated, if
    /// [`Self::with_generated_project`] added one; it still needs saving
    /// (see [`crate::start::wizard::save_config`]).
    pub fn generated_project(&self) -> Option<&GeneratedConfig> {
        self.generated.as_ref()
    }

    /// Merge all layers in precedence order, each over the config values
    /// of its packs, apply `overrides`, and parse and validate the result.
    ///
    /// The names of the `presets` lists must be released names (see
    /// [`legacy_presets::validate_names`]). Each layer's `[packs]` table
    /// is read on its own and the entries merge per pack (see
    /// [`pack_entries`]). The config values of the enabled packs (a
    /// `config.lua` runs here, on every resolve) must not conflict, across
    /// all layers (see [`pack_conflicts`]). The documents of the `presets`
    /// lists apply first, then each layer (see [`merge_config`]): the
    /// config values of the packs whose highest entry is in that layer,
    /// then the layer's own values (without `packs` or `presets`). So a
    /// pack overrides the layers below its highest entry (a project pack
    /// overrides the user files), and the layer of that entry overrides
    /// the pack.
    pub async fn resolve(
        &self,
        packs: &PackManager,
        overrides: &ConfigOverrides,
    ) -> anyhow::Result<ResolvedConfig> {
        let known = packs.builtin();
        let legacy_base = self.legacy_base(&known)?;
        let mut problems = Vec::new();
        let mut entries = Vec::new();
        let mut plain_layers = Vec::new();
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

    /// The layers, lowest precedence first.
    fn layers(&self) -> impl Iterator<Item = &Layer> {
        self.user.iter().chain(&self.local).chain(&self.project)
    }

    /// The documents of the `presets` lists of all layers, merged (see
    /// [`legacy_presets::expand`]); they apply beneath every layer and its
    /// packs. A name that is not a released one is an error; `known` (the
    /// built-in packs) gives its hint.
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

    /// The config of `user`, `local` and `project` (each lowest precedence
    /// first), with no generated project config yet.
    fn new(user: Vec<Layer>, local: Option<Layer>, project: Vec<Layer>) -> Self {
        Self {
            user,
            local,
            project,
            generated: None,
        }
    }

    /// Load the config files of `project_root` with `home` as the home
    /// directory (see [`files::discover_in`]).
    fn load_from(home: &Path, project_root: &Path) -> anyhow::Result<Self> {
        let files = files::discover_in(home, project_root)?;
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

/// The image of the user files (see [`LayeredConfig::user_image`]).
#[derive(Clone)]
pub struct UserImage {
    /// The image reference (`image`, or the `name` of `[vm.image]`).
    pub name: String,
    /// `vm.image` as the file has it.
    pub value: serde_json::Value,
}

/// A resolved config and the enabled packs.
pub struct ResolvedConfig {
    /// Everything applied (the `--network` policy too, when it is set).
    pub values: ConfigValues,
    /// Every enabled `[packs]` entry, in pack order.
    pub packs: Vec<ConfiguredPack>,
}

impl ResolvedConfig {
    /// The config of the install boot (see
    /// [`crate::packs::install::phase::install_config`]).
    pub(crate) fn install_config(&self) -> anyhow::Result<ConfigValues> {
        crate::packs::install::phase::install_config(self.values.clone())
    }
}

/// The config values of `packs` (in their order), each env-normalized
/// (see [`normalize_env`]). A pack whose config fails (its `config.lua`)
/// and a value that two packs set differently (see [`pack_conflicts`])
/// are problems, one line each.
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

/// One layer's part of [`merge_config`]: its own values (without `packs`
/// and `presets`), and the names of the packs that its `[packs]` table
/// has entries for.
struct PlainLayer {
    value: serde_json::Value,
    pack_names: Vec<String>,
}

/// Merge `legacy_base` (the documents of the `presets` lists), then each
/// of `layers` (lowest precedence first): the config values of the packs
/// whose highest entry is in that layer, then the layer's own values.
/// `pack_values` are the config values of `packs`, in the same order.
/// Parse and validate the result.
fn merge_config(
    legacy_base: serde_json::Value,
    layers: Vec<PlainLayer>,
    packs: &[ConfiguredPack],
    pack_values: Vec<serde_json::Value>,
) -> anyhow::Result<ConfigValues> {
    // Every configured pack has an entry, so it has a layer.
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
