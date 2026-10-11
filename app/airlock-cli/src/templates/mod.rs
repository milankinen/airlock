//! Sandbox templates.
//!
//! A template is a stored sandbox disk with installed packs, and the config
//! that made it. The user creates a template with the setup wizard. A new
//! sandbox can start from a template: it gets a copy of the template disk
//! and installs only the packs that the template does not have.
//!
//! The template config pins the image digest when the image source has a
//! registry digest for it. Thus a moved image tag (for example
//! `alpine:latest`) does not make the sandbox ask to re-create itself.

pub mod create;
pub mod seed;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::generated::Clipboard;
use crate::messages;
use crate::packs::install::state::STATE_FILE;
use crate::vm::disk::{DISK_FILE, DISK_ID_FILE};

/// Directory of the templates in the data directory.
pub const TEMPLATES_DIR: &str = "templates";
/// The config file of a template.
pub const CONFIG_FILE: &str = "airlock.toml";
/// The image reference file of a template. It is a hardlink to the cached
/// image, thus the image cleanup keeps the image.
pub const IMAGE_FILE: &str = "image";
/// The CA keypair file of a template. Pack installs can write the CA
/// certificate into the CA bundles on the disk, thus a sandbox from the
/// template must use the same CA.
pub const CA_FILE: &str = "ca.json";
/// The files of a template. Nothing else is kept.
pub const FILES: [&str; 6] = [
    CONFIG_FILE,
    IMAGE_FILE,
    DISK_FILE,
    DISK_ID_FILE,
    STATE_FILE,
    CA_FILE,
];
/// Longest template name.
const MAX_NAME_LEN: usize = 64;

/// A stored template.
#[derive(Clone, Debug)]
pub struct Template {
    /// Template name (see [`is_valid_name`]).
    pub name: String,
    /// Template directory `<data_dir>/templates/<name>`.
    pub dir: PathBuf,
    /// The config of the template.
    pub config: TemplateConfig,
}

/// The config of a template, as the template files have it.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateConfig {
    /// The TOML text of the config file. A sandbox config from the template
    /// starts with this text.
    pub toml: String,
    /// The `[packs]` entries, sorted by name.
    pub packs: Vec<TemplatePack>,
    /// `vm.image` name, with the pinned digest.
    pub image: Option<String>,
    /// `[clipboard]`, if set.
    pub clipboard: Option<Clipboard>,
}

/// A `[packs]` entry of a template config.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplatePack {
    /// Pack name.
    pub name: String,
    /// Pack version.
    pub version: String,
    /// Arg values as the file has them, by key.
    pub args: BTreeMap<String, serde_json::Value>,
}

impl TemplateConfig {
    /// Parse the config file text `toml` of a template.
    /// Returns:
    ///   The config, or error if the text is not valid TOML or a `[packs]`
    ///   entry has no version.
    pub fn parse(toml: &str) -> anyhow::Result<Self> {
        let value: serde_json::Value = toml::from_str(toml)?;
        let mut packs = Vec::new();
        if let Some(table) = value.get("packs").and_then(|p| p.as_object()) {
            for (name, entry) in table {
                let version = match entry.get("version") {
                    Some(serde_json::Value::String(v)) => v.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    _ => anyhow::bail!(messages::templates::pack_has_no_version(name)),
                };
                let args = entry
                    .get("args")
                    .and_then(|a| a.as_object())
                    .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                packs.push(TemplatePack {
                    name: name.clone(),
                    version,
                    args,
                });
            }
        }
        let image = value.pointer("/vm/image").and_then(|image| {
            image
                .as_str()
                .or_else(|| image.get("name")?.as_str())
                .map(str::to_string)
        });
        let clipboard = value.get("clipboard").map(|c| Clipboard {
            copy: c.get("copy").and_then(serde_json::Value::as_bool) == Some(true),
            paste: c.get("paste").and_then(serde_json::Value::as_bool) == Some(true),
        });
        Ok(Self {
            toml: toml.to_string(),
            packs,
            image,
            clipboard,
        })
    }
}

/// Return the templates directory of the data directory `data_dir`.
pub fn templates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(TEMPLATES_DIR)
}

/// Return true if `name` is a valid template name: 1 to 64 ASCII letters,
/// digits, `-`, `_` or `.`. The first character is a letter or a digit.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name.len() <= MAX_NAME_LEN
}

/// Check the template name `name`.
/// Returns:
///   Error with the naming rule if the name is not valid.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        is_valid_name(name),
        messages::templates::invalid_name(name, MAX_NAME_LEN)
    );
    Ok(())
}

/// Read the template `name` in `data_dir`.
/// Returns:
///   The template, or error if it does not exist or its config is not
///   valid.
pub fn find(data_dir: &Path, name: &str) -> anyhow::Result<Template> {
    check_name(name)?;
    let dir = templates_dir(data_dir).join(name);
    anyhow::ensure!(dir.is_dir(), messages::templates::template_missing(name));
    read(name, dir)
}

/// Read the template `name` in its directory `dir`.
fn read(name: &str, dir: PathBuf) -> anyhow::Result<Template> {
    let path = dir.join(CONFIG_FILE);
    let toml = std::fs::read_to_string(&path).map_err(|e| {
        anyhow::anyhow!(messages::templates::read_config_failed(
            name,
            path.display(),
            e
        ))
    })?;
    let config = TemplateConfig::parse(&toml)
        .map_err(|e| anyhow::anyhow!(messages::templates::config_invalid(name, e)))?;
    Ok(Template {
        name: name.to_string(),
        dir,
        config,
    })
}

/// Read all templates in `data_dir`.
/// Returns:
///   The templates sorted by name. A template that cannot be read is not in
///   the list (the log tells why). Directories with a name that is not
///   valid are build directories, and are not in the list.
pub fn list(data_dir: &Path) -> Vec<Template> {
    let Ok(entries) = std::fs::read_dir(templates_dir(data_dir)) else {
        return Vec::new();
    };
    let mut templates: Vec<Template> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !is_valid_name(&name) {
                return None;
            }
            read(&name, e.path())
                .inspect_err(|e| tracing::warn!("templates: {e:#}"))
                .ok()
        })
        .collect();
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    templates
}

/// Remove the template `name` from `data_dir`. The caller removes the
/// unused images after this (see [`crate::oci::gc_sweep`]).
/// Returns:
///   Error if the template does not exist or the removal fails.
pub fn remove(data_dir: &Path, name: &str) -> anyhow::Result<()> {
    check_name(name)?;
    let dir = templates_dir(data_dir).join(name);
    anyhow::ensure!(dir.is_dir(), messages::templates::template_missing(name));
    std::fs::remove_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!(messages::templates::remove_failed(dir.display(), e)))?;
    Ok(())
}
