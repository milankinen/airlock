//! New config file of the setup wizard.
//!
//! Makes and saves the project config file that the setup wizard creates.
//! The file contains the selected packs, and optionally an image and
//! clipboard access.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::packs::ArgValue;
use crate::project;

/// Location of a new project config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `airlock.toml`, next to the project files (under version control).
    Project,
    /// `.airlock/airlock.toml` (local, because git ignores `.airlock/`).
    Local,
}

impl Target {
    /// Path of the target file, relative to the project directory.
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
    /// Path of the `target` file in the project.
    pub path: PathBuf,
    /// TOML content of the file:
    ///  * the pack entries (`[packs] <name> = { version = "1", args = { … } }`,
    ///    `args` only if there are args)
    ///  * `vm.image` and `[clipboard]`, if given
    ///
    /// Empty if none of them is given.
    pub toml: String,
}

impl GeneratedConfig {
    /// Make a new config file.
    /// Args:
    ///  - `host_cwd`: Project directory
    ///  - `target`: Location of the file
    ///  - `entries`: Pack entries
    ///  - `image`: `vm.image` as it is in a config file, if any
    ///  - `clipboard`: Clipboard access, if any
    pub fn new(
        host_cwd: &Path,
        target: Target,
        entries: &[NewEntry<'_>],
        image: Option<&serde_json::Value>,
        clipboard: Option<Clipboard>,
    ) -> Self {
        Self {
            target,
            path: target.path(host_cwd),
            toml: render(entries, image, clipboard),
        }
    }

    /// Write the file as a new file. If a file appeared in the meantime,
    /// this is an error and the file is not overwritten. For the local
    /// file, `.airlock/.gitignore` is created first.
    pub fn save(&self) -> anyhow::Result<()> {
        if self.target == Target::Local {
            let host_cwd = self.path.parent().and_then(Path::parent).ok_or_else(|| {
                anyhow::anyhow!("{} has no project directory", self.path.display())
            })?;
            project::ensure_cache_dir(host_cwd)?;
        }
        create_new(&self.path, &self.toml).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => anyhow::anyhow!(
                "{} appeared while the setup questions were open; it was not changed",
                self.path.display()
            ),
            _ => anyhow::anyhow!("create {}: {e}", self.path.display()),
        })
    }
}

/// Make the content of a new config file (see [`GeneratedConfig::toml`]).
fn render(
    entries: &[NewEntry<'_>],
    image: Option<&serde_json::Value>,
    clipboard: Option<Clipboard>,
) -> String {
    let mut doc = toml_edit::DocumentMut::new();
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
        doc["packs"] = toml_edit::Item::Table(table);
    }
    if let Some(image) = image {
        match vm_image(image) {
            Ok(vm) => doc["vm"] = vm,
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
