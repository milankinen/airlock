//! The new config file of the setup wizard.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::packs::ArgValue;
use crate::project;

/// Where a new project config goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `airlock.toml`, next to the project files (version-controlled).
    Project,
    /// `.airlock/airlock.toml` (local; `.airlock/` is git-ignored).
    Local,
}

impl Target {
    /// The file of the target, relative to the project directory.
    pub fn file(self) -> &'static str {
        match self {
            Target::Project => "airlock.toml",
            Target::Local => ".airlock/airlock.toml",
        }
    }

    pub fn path(self, host_cwd: &Path) -> PathBuf {
        host_cwd.join(self.file())
    }
}

/// What the sandbox may do with the host clipboard (`[clipboard]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clipboard {
    /// Write to it (`copy`).
    pub copy: bool,
    /// Read it (`paste`).
    pub paste: bool,
}

/// A pack entry of a new config file.
pub struct NewEntry<'a> {
    pub name: &'a str,
    pub version: &'a str,
    /// The arg values to write, by key.
    pub args: Vec<(&'a str, &'a ArgValue)>,
}

/// A new config file, not written yet.
#[derive(Clone)]
pub struct GeneratedConfig {
    pub target: Target,
    /// The file of `target` in the project.
    pub path: PathBuf,
    /// The content: the pack entries
    /// (`[packs] <name> = { version = "1", args = { … } }`, `args` only
    /// when there are any), `vm.image` and `[clipboard]` when they are
    /// given. Empty without any of them.
    pub toml: String,
}

impl GeneratedConfig {
    /// The config file of `target` in the project `host_cwd`, with the
    /// pack `entries`, and `image` (`vm.image`, as a config file has it)
    /// and `clipboard`, if given.
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

    /// Write the file as a new file. A file that appeared in the meantime
    /// is an error, not overwritten. The local file gets
    /// `.airlock/.gitignore` first.
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

/// The content of a new config file (see [`GeneratedConfig::toml`]).
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

/// The `[vm]` table with `image` (as a config file has it).
fn vm_image(image: &serde_json::Value) -> anyhow::Result<toml_edit::Item> {
    let text = toml::to_string(&serde_json::json!({ "vm": { "image": image } }))?;
    let doc: toml_edit::DocumentMut = text.parse()?;
    Ok(doc["vm"].clone())
}

/// An arg value as a TOML value.
fn toml_value(value: &ArgValue) -> toml_edit::Value {
    match value {
        ArgValue::Bool(b) => (*b).into(),
        ArgValue::Text(s) => s.as_str().into(),
    }
}

/// Create the file `path` with `content`; an existing file is an error.
fn create_new(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(content.as_bytes())
}
