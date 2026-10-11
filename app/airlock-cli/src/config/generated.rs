//! New config file of the setup wizard.
//!
//! Makes and saves the project config file that the setup wizard creates.
//! The file contains the selected packs, and optionally an image and
//! clipboard access. A config from a template starts with the template
//! config and adds the other selections to it.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::packs::ArgValue;
use crate::project;
use crate::templates::Template;

/// Location of a new project config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `airlock.toml`, next to the project files (under version control).
    Project,
    /// `airlock.toml` in the local config directory: the sandbox directory
    /// of a sandbox in the data directory, else `.airlock/` (git ignores
    /// it). See [`crate::sandboxes::local_config_dir`].
    Local,
}

impl Target {
    /// Path of the target file, relative to the project directory. For
    /// [`Target::Local`], this is the place in a project sandbox.
    pub fn file(self) -> &'static str {
        match self {
            Target::Project => "airlock.toml",
            Target::Local => ".airlock/airlock.toml",
        }
    }

    /// Path of the target file in the project directory `host_cwd`.
    pub fn path(self, host_cwd: &Path) -> PathBuf {
        host_cwd.join(self.file())
    }
}

/// Host clipboard access of the sandbox (`[clipboard]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clipboard {
    /// The sandbox can write to the clipboard (`copy`).
    pub copy: bool,
    /// The sandbox can read the clipboard (`paste`).
    pub paste: bool,
}

/// A pack entry of a new config file.
pub struct NewEntry<'a> {
    /// Pack name.
    pub name: &'a str,
    /// Pack version.
    pub version: &'a str,
    /// Arg values to write, by key.
    pub args: Vec<(&'a str, &'a ArgValue)>,
}

/// A new config file that is not written yet.
#[derive(Clone)]
pub struct GeneratedConfig {
    /// Location of the file.
    pub target: Target,
    /// Path of the `target` file in the project. For [`Target::Local`],
    /// [`Self::save`] can write the file to the sandbox directory instead.
    pub path: PathBuf,
    /// TOML content of the file:
    ///  * the template config, if a template is given
    ///  * the pack entries (`[packs] <name> = { version = "1", args = { … } }`,
    ///    `args` only if there are args)
    ///  * `vm.image` and `[clipboard]`, if given
    ///
    /// Empty if none of them is given.
    pub toml: String,
    /// Name of the template that the config starts from, if any. The
    /// sandbox gets a copy of the template disk.
    pub template: Option<String>,
}

impl GeneratedConfig {
    /// Make a new config file.
    /// Args:
    ///  - `host_cwd`: Project directory
    ///  - `target`: Location of the file
    ///  - `entries`: Pack entries
    ///  - `image`: `vm.image` as it is in a config file, if any
    ///  - `clipboard`: Clipboard access, if any. Replaces the `[clipboard]`
    ///    of the template.
    ///  - `template`: The template that the config starts from, if any.
    ///    `entries` are the packs that the template does not have.
    pub fn new(
        host_cwd: &Path,
        target: Target,
        entries: &[NewEntry<'_>],
        image: Option<&serde_json::Value>,
        clipboard: Option<Clipboard>,
        template: Option<&Template>,
    ) -> Self {
        Self {
            target,
            path: target.path(host_cwd),
            toml: render(template, entries, image, clipboard),
            template: template.map(|t| t.name.clone()),
        }
    }

    /// Write the file as a new file. If a file appeared in the meantime,
    /// this is an error and the file is not overwritten.
    /// Args:
    ///  - `local_dir`: Local config directory of the project (see
    ///    [`crate::sandboxes::local_config_dir`]). The local file goes
    ///    there. For `.airlock/`, `.airlock/.gitignore` is created first.
    ///
    /// Returns:
    ///   The path of the written file.
    pub fn save(&self, local_dir: &Path) -> anyhow::Result<PathBuf> {
        let path = match self.target {
            Target::Project => self.path.clone(),
            Target::Local => {
                let in_project = self.path.parent() == Some(local_dir);
                if in_project {
                    let host_cwd = local_dir.parent().ok_or_else(|| {
                        anyhow::anyhow!(crate::messages::config::no_project_directory(
                            local_dir.display()
                        ))
                    })?;
                    project::ensure_cache_dir(host_cwd)?;
                }
                local_dir.join("airlock.toml")
            }
        };
        create_new(&path, &self.toml).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow::anyhow!(
                crate::messages::config::config_appeared_during_setup(path.display())
            ),
            _ => anyhow::anyhow!(crate::messages::config::create_config_failed(
                path.display(),
                e
            )),
        })?;
        Ok(path)
    }
}

/// Make the content of a new config file (see [`GeneratedConfig::toml`]).
fn render(
    template: Option<&Template>,
    entries: &[NewEntry<'_>],
    image: Option<&serde_json::Value>,
    clipboard: Option<Clipboard>,
) -> String {
    let mut doc = template.map_or_else(toml_edit::DocumentMut::new, |t| {
        t.config.toml.parse().unwrap_or_else(|e| {
            tracing::warn!("config: template {} is not used: {e}", t.name);
            toml_edit::DocumentMut::new()
        })
    });
    if !entries.is_empty() {
        let mut table = toml_edit::Table::new();
        for new in entries {
            let mut entry = toml_edit::InlineTable::new();
            entry.insert("version", new.version.into());
            if !new.args.is_empty() {
                let mut values = toml_edit::InlineTable::new();
                for (key, value) in &new.args {
                    values.insert(*key, toml_value(value));
                }
                entry.insert("args", toml_edit::Value::InlineTable(values));
            }
            table.insert(new.name, toml_edit::value(entry));
        }
        // Add to the `[packs]` of the template, if it has one.
        match doc
            .get_mut("packs")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            Some(packs) => {
                for (name, entry) in table {
                    packs.insert(&name, entry);
                }
            }
            None => doc["packs"] = toml_edit::Item::Table(table),
        }
    }
    if let Some(image) = image {
        match vm_image(image) {
            // Keep the other `[vm]` settings of the template.
            Ok(vm) => match doc
                .get_mut("vm")
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                Some(table) => {
                    table.insert("image", vm["image"].clone());
                }
                None => doc["vm"] = vm,
            },
            Err(e) => tracing::warn!("config: the user image is not written: {e:#}"),
        }
    }
    if let Some(clipboard) = clipboard {
        let mut table = toml_edit::Table::new();
        table.insert("copy", toml_edit::value(clipboard.copy));
        table.insert("paste", toml_edit::value(clipboard.paste));
        doc["clipboard"] = toml_edit::Item::Table(table);
    }
    doc.to_string()
}

/// Make the `[vm]` table with `image` (as it is in a config file).
fn vm_image(image: &serde_json::Value) -> anyhow::Result<toml_edit::Item> {
    let text = toml::to_string(&serde_json::json!({ "vm": { "image": image } }))?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    Ok(doc["vm"].clone())
}

/// Convert an arg value to a TOML value.
fn toml_value(value: &ArgValue) -> toml_edit::Value {
    match value {
        ArgValue::Bool(b) => (*b).into(),
        ArgValue::Text(s) => s.as_str().into(),
    }
}

/// Create the file `path` with `content`. An existing file is an error.
fn create_new(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(content.as_bytes())
}
