//! Config file discovery and parsing.

use std::path::{Path, PathBuf};

pub(crate) const EXTENSIONS: &[&str] = &["toml", "json", "yaml", "yml"];

/// The config files found by [`discover_in`], each as its path and parsed
/// value, lowest precedence first per slot.
pub(crate) struct DiscoveredFiles {
    /// `~/.airlock/airlock.<ext>`, `~/.airlock/config.<ext>`, `~/.airlock.<ext>`
    pub user: Vec<(PathBuf, serde_json::Value)>,
    /// `<project_root>/.airlock/airlock.<ext>`
    pub local: Option<(PathBuf, serde_json::Value)>,
    /// `<project_root>/airlock.<ext>`, `<project_root>/airlock.local.<ext>`
    pub project: Vec<(PathBuf, serde_json::Value)>,
}

/// Find and parse the config files of `project_root` with `home` as the
/// home directory.
///
/// Files are loaded in order (later overrides former):
/// 1. `~/.airlock/airlock.<ext>`
/// 2. `~/.airlock/config.<ext>`
/// 3. `~/.airlock.<ext>`
/// 4. `<project_root>/.airlock/airlock.<ext>` (local project file)
/// 5. `<project_root>/airlock.<ext>`
/// 6. `<project_root>/airlock.local.<ext>`
///
/// When the project root is the home directory, `.airlock/airlock.<ext>`
/// is the local project file only, not also a user file.
///
/// Supported formats: TOML, JSON, YAML. For each slot, the first matching
/// extension (`toml` → `json` → `yaml` → `yml`) wins.
pub(crate) fn discover_in(home: &Path, project_root: &Path) -> anyhow::Result<DiscoveredFiles> {
    let local_base = project_root.join(".airlock/airlock");
    let home_base = home.join(".airlock/airlock");
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let home_is_project = canonical(home) == canonical(project_root);

    let mut user = Vec::new();
    if !home_is_project {
        user.extend(load_first(&home_base)?);
    }
    for base in [home.join(".airlock/config"), home.join(".airlock")] {
        user.extend(load_first(&base)?);
    }

    let local = load_first(&local_base)?;

    let mut project = Vec::new();
    for base in [
        project_root.join("airlock"),
        project_root.join("airlock.local"),
    ] {
        project.extend(load_first(&base)?);
    }

    Ok(DiscoveredFiles {
        user,
        local,
        project,
    })
}

/// Try each supported extension for `base` and parse the first file found.
pub(super) fn load_first(base: &Path) -> anyhow::Result<Option<(PathBuf, serde_json::Value)>> {
    for ext in EXTENSIONS {
        let path = PathBuf::from(format!("{}.{ext}", base.display()));
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            // Only a genuinely absent file falls through to the next
            // extension. Any other error (permission, IO, a directory in
            // the file's place) means the config exists but can't be
            // read — fail closed so the sandbox never silently drops the
            // user's policy in favor of permissive defaults.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(anyhow::anyhow!("read config file {}: {e}", path.display()));
            }
        };
        let value = parse_file(&path, &content)?;
        return Ok(Some((path, value)));
    }
    Ok(None)
}

pub(crate) fn parse_file(path: &Path, content: &str) -> anyhow::Result<serde_json::Value> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => {
            toml::from_str(content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        Some("json") => {
            serde_json::from_str(content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        Some("yaml" | "yml") => {
            serde_yaml::from_str(content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        _ => anyhow::bail!("unsupported config format: {}", path.display()),
    }
}
