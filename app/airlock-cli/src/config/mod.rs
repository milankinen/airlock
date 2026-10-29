//! Layered airlock configuration with pack support.
//!
//! Reads the user and project config files and merges them into one
//! validated config. The config of the enabled packs merges in with the
//! files. The rest of the program uses only the resolved config.
//!
//! Also handles the `[packs]` tables, the legacy `presets` lists, the
//! new config file that the setup wizard creates, and the config export.

pub(crate) mod config_values;
pub(crate) mod de;
pub(crate) mod export;
pub(crate) mod files;
pub(crate) mod generated;
pub(crate) mod legacy_presets;
pub(crate) mod merge;
pub(crate) mod pack_entries;
#[cfg(test)]
mod tests;

use crate::config::config_values::{ConfigValues, Policy};
use crate::config::generated::GeneratedConfig;
use crate::config::merge::{merge_json, normalize_env, pack_conflicts};
use crate::data_dir::DataDir;
use crate::packs::approval::{self, ApprovalMode, Approved};
use crate::packs::{ConfiguredPack, Pack, PackManager, PackRef};
use crate::util::VerifiedPath;

/// Load the config files of the project `project_root`. The user files come
/// from the home directory of the user, and the local project file from
/// `local_dir` (see [`crate::sandboxes::local_config_dir`]).
pub fn load(
    project_root: &VerifiedPath,
    local_dir: &VerifiedPath,
) -> anyhow::Result<LayeredConfig> {
    let home = VerifiedPath::home_dir()?;
    LayeredConfig::load_from(&home, project_root, local_dir)
}

/// One config file (or in-memory document) before merging.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    fn from_file((path, value): (String, serde_json::Value)) -> anyhow::Result<Self> {
        let layer = Self::new(path, value)?;
        tracing::debug!("config: loaded {}", layer.origin);
        tracing::trace!("config: {}: {}", layer.origin, layer.value);
        Ok(layer)
    }

    /// Make the layer of a discovered user file (see [`Self::from_file`]).
    /// A `packs` key is an error, because only project-level files can
    /// enable packs.
    fn from_user_file(file: (String, serde_json::Value)) -> anyhow::Result<Self> {
        let layer = Self::from_file(file)?;
        anyhow::ensure!(
            layer.value.get("packs").is_none(),
            crate::messages::config::packs_only_in_project_files(&layer.origin)
        );
        Ok(layer)
    }
}

/// What [`LayeredConfig::resolve`] needs besides the config files.
#[derive(Clone, Copy)]
pub struct ResolveInput<'a> {
    /// Airlock data directory, for the pack mount directories.
    pub data_dir: &'a DataDir,
    /// Known packs.
    pub packs: &'a PackManager,
    /// Command line overrides.
    pub overrides: &'a ConfigOverrides,
    /// Project root directory, for the project packs (`./path`). It is the
    /// directory where the project files are. `None` if there is no
    /// project, for example in a template build. Then a project pack is an
    /// error.
    pub project_root: Option<&'a VerifiedPath>,
    /// How to get the approval of a new or changed user pack or project
    /// pack (see [`approval`]).
    pub approval: ApprovalMode,
}

/// Command line overrides for [`LayeredConfig::resolve`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigOverrides {
    /// `airlock start --network <POLICY>`.
    pub network_policy: Option<Policy>,
}

/// The config files of a project, lowest precedence first:
/// user files < local project file < project files.
///
/// The serialized form has the layers, without the generated project
/// config. The config change check keeps it (see
/// [`crate::start::config_change`]).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(skip)]
    generated: Option<GeneratedConfig>,
}

impl LayeredConfig {
    /// True if the config includes deprecated user-level config files.
    pub fn has_user_files(&self) -> bool {
        !self.user.is_empty()
    }

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
            crate::messages::config::PROJECT_HAS_CONFIG_ALREADY
        );
        let origin = crate::messages::config::setup_wizard_origin(generated.path());
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
    ///
    /// The project packs (`./path`) load from the project root on each
    /// resolve.
    /// A user pack or a project pack needs the approval of the user before
    /// its `config.lua` runs (see [`approval`]).
    /// Args:
    ///  - `input`: Data directory, known packs, overrides, project root and
    ///    approval mode
    ///
    /// Returns:
    ///   The validated config and the enabled packs. An error lists all
    ///   problems, for example an unknown preset name, an invalid
    ///   `[packs]` entry, a missing project pack, a failed `config.lua`, or
    ///   two packs that set a value differently. A pack without approval
    ///   gives an approval error (see [`approval::approve`]).
    pub async fn resolve(&self, input: &ResolveInput<'_>) -> anyhow::Result<ResolvedConfig> {
        // The steps keep the pack files in memory. The approval covers the
        // same text that the evaluation runs, so a file change after the
        // approval has no effect.
        let selected = self.select(input.packs, input.project_root).await?;
        let approved = approval::approve(input.data_dir, &selected.packs, input.approval)?;
        selected.evaluate(input.data_dir, *input.overrides, &approved)
    }

    /// Read the layers and their `[packs]` tables, and load the packs. No
    /// `config.lua` runs here.
    /// Args:
    ///  - `packs`: Known packs
    ///  - `project_root`: Project root directory for the project packs
    ///
    /// Returns:
    ///   The layers and the configured packs, or an error that lists all
    ///   problems.
    async fn select(
        &self,
        packs: &PackManager,
        project_root: Option<&VerifiedPath>,
    ) -> anyhow::Result<Selected> {
        let mut known = packs.offered();
        // The names of the `presets` lists must be released names (see
        // `legacy_presets::validate_names`).
        let legacy_base = self.legacy_base(&known)?;
        let mut problems = Vec::new();
        let mut entries = Vec::new();
        let mut layers = Vec::new();
        // Read the `[packs]` table of each layer separately. The entries
        // merge for each pack (see `pack_entries`).
        for layer in self.layers() {
            tracing::trace!("config: reading {}", layer.origin);
            let mut value = layer.value.clone();
            let layer_entries =
                pack_entries::read_layer_packs(&layer.origin, &mut value, &known, &mut problems);
            let pack_names = layer_entries.iter().map(|e| e.name.clone()).collect();
            entries.extend(layer_entries);
            layers.push(PlainLayer { value, pack_names });
        }
        add_project_packs(&mut known, &entries, project_root, &mut problems);
        let configured = pack_entries::configure_packs(entries, packs, &known, &mut problems).await;
        if !problems.is_empty() {
            anyhow::bail!(crate::messages::config::invalid_configuration_problems(
                problems.join("\n")
            ));
        }
        Ok(Selected {
            legacy_base,
            layers,
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
        home: &VerifiedPath,
        project_root: &VerifiedPath,
        local_dir: &VerifiedPath,
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
    /// were loaded from the user, local and project slots. The project slot
    /// files are project root files.
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
    /// The network policy of the config, without the `--network` policy.
    pub configured_policy: Policy,
}

impl ResolvedConfig {
    /// Make the config of the install boot (see
    /// [`crate::packs::install::phase::install_config`]).
    pub(crate) fn install_config(&self) -> anyhow::Result<ConfigValues> {
        crate::packs::install::phase::install_config(self.values.clone())
    }
}

/// Maximum number of project packs that one config can name.
const MAX_PROJECT_PACKS: usize = 32;

/// Load the project packs that the entries name and add them to the known
/// packs. Each project pack loads one time, also if more than one file
/// names it. At most [`MAX_PROJECT_PACKS`] packs load.
/// Args:
///  - `known`: Packs that an entry can name. The project packs are added
///    after the other packs of the same kind.
///  - `entries`: Entries of all files
///  - `project_root`: Project root directory, or `None` if there is no
///    project
///  - `problems`: Receives a problem for each project pack that does not
///    load
fn add_project_packs(
    known: &mut Vec<Pack>,
    entries: &[pack_entries::PackEntry],
    project_root: Option<&VerifiedPath>,
    problems: &mut Vec<String>,
) {
    let mut loaded = false;
    let mut tried = std::collections::BTreeSet::new();
    for entry in entries {
        let Some(id) = crate::packs::PackId::parse(&entry.name)
            .filter(|id| matches!(id.pack_ref(), PackRef::Project { .. }))
        else {
            continue;
        };
        if !tried.insert(id.clone()) {
            continue;
        }
        // Each project pack reads files from the disk. The guest can write
        // the project files, so limit the number.
        if tried.len() > MAX_PROJECT_PACKS {
            problems.push(format!(
                "* {}",
                crate::messages::config::project_packs_too_many(MAX_PROJECT_PACKS)
            ));
            break;
        }
        let Some(root) = project_root else {
            problems.push(format!(
                "* {}",
                crate::messages::config::PROJECT_PACK_NO_PROJECT
            ));
            continue;
        };
        match crate::packs::load_project_pack(&id, root) {
            Ok(pack) => {
                known.push(pack);
                loaded = true;
            }
            Err(e) => problems.push(format!("* {e:#}")),
        }
    }
    if loaded {
        // The sort is stable. Thus the project packs stay after the other
        // packs of the same kind.
        known.sort_by_key(|pack| pack.metadata().kind);
    }
}

/// Get the config values of the packs, and set the setup env of each pack.
/// Args:
///  - `data_dir`: Airlock data directory, for the pack mount directories
///    and the setup env hash key (see [`crate::packs::EnvMacKey`])
///  - `packs`: Configured packs. Each pack gets its setup env.
///  - `approved`: One approval token for each pack, in the same order
///
/// Returns:
///   The config values in pack order, with normalized env (see
///   [`normalize_env`]). Or the problems, one line each: a pack whose
///   `config.lua` fails, a setup env hash key that cannot be read, or a
///   value that two packs set differently (see [`pack_conflicts`]).
fn pack_configs(
    data_dir: &DataDir,
    packs: &mut [ConfiguredPack],
    approved: &[Approved],
) -> Result<Vec<serde_json::Value>, Vec<String>> {
    debug_assert_eq!(packs.len(), approved.len(), "one token for each pack");
    let mut docs = Vec::new();
    let mut problems = Vec::new();
    // Read the hash key only when a pack has a setup env. Thus configs with
    // no setup env do not make the key file.
    let mut key = None;
    for (pack, approved) in packs.iter_mut().zip(approved) {
        let metadata = pack.metadata();
        tracing::debug!("config: applying pack `{}`", metadata.id_with_version());
        match pack.config_values(data_dir, approved) {
            Ok((mut value, setup_env)) => {
                normalize_env(&mut value);
                docs.push((metadata.id.to_string(), value));
                if !setup_env.is_empty() && key.is_none() {
                    match crate::packs::EnvMacKey::load_or_create(data_dir) {
                        Ok(loaded) => key = Some(loaded),
                        Err(e) => {
                            problems.push(format!("* {e:#}"));
                            continue;
                        }
                    }
                }
                pack.set_setup_env(setup_env, key.as_ref());
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

/// The layers and the packs of a config, before any `config.lua` runs (see
/// [`LayeredConfig::resolve`]).
struct Selected {
    /// Merged documents of the `presets` lists.
    legacy_base: serde_json::Value,
    /// Layers, lowest precedence first.
    layers: Vec<PlainLayer>,
    /// All enabled `[packs]` entries, in pack order.
    packs: Vec<ConfiguredPack>,
}

impl Selected {
    /// Run the `config.lua` of the packs and merge the result.
    /// Args:
    ///  - `data_dir`: Airlock data directory, for the pack mount directories
    ///  - `overrides`: Command line overrides
    ///  - `approved`: One approval token for each pack, in pack order
    ///
    /// Returns:
    ///   The resolved config, or an error that lists the problems of the
    ///   packs (see [`pack_configs`]) or of the merged config.
    fn evaluate(
        mut self,
        data_dir: &DataDir,
        overrides: ConfigOverrides,
        approved: &[Approved],
    ) -> anyhow::Result<ResolvedConfig> {
        // A `config.lua` runs here, on each resolve. The values of the
        // enabled packs must not conflict across all layers.
        let pack_values =
            pack_configs(data_dir, &mut self.packs, approved).map_err(|problems| {
                anyhow::anyhow!(crate::messages::config::invalid_configuration_problems(
                    problems.join("\n")
                ))
            })?;
        let mut values = merge_config(self.legacy_base, self.layers, &self.packs, pack_values)?;
        let configured_policy = values.network.policy;
        if let Some(policy) = overrides.network_policy {
            tracing::info!("network policy overridden by --network: {}", policy.label());
            values.network.policy = policy;
        }
        Ok(ResolvedConfig {
            values,
            packs: self.packs,
            configured_policy,
        })
    }
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
        let id = &pack.metadata().id;
        if let Some(top) = layers
            .iter()
            .rposition(|l| l.pack_names.iter().any(|name| *id == *name))
        {
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
