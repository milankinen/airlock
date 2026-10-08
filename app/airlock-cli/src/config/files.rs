//! Config file discovery and parsing.
//!
//! Finds the user and project config files of a project and parses them.
//! Config files can be TOML, JSON or YAML.

use std::path::{Path, PathBuf};

/// Supported config file extensions, in priority order.
pub(crate) const EXTENSIONS: &[&str] = &["toml", "json", "yaml", "yml"];

/// Config files that [`discover_in`] found, each as its path and parsed
/// value. Each slot is in order of precedence, lowest first.
pub(crate) struct DiscoveredFiles {
    /// `~/.airlock/airlock.<ext>`, `~/.airlock/config.<ext>`, `~/.airlock.<ext>`
    pub user: Vec<(PathBuf, serde_json::Value)>,
    /// `<local_dir>/airlock.<ext>` (see [`discover_in`])
    pub local: Option<(PathBuf, serde_json::Value)>,
    /// `<project_root>/airlock.<ext>`, `<project_root>/airlock.local.<ext>`
    pub project: Vec<(PathBuf, serde_json::Value)>,
}

/// Find and parse the config files of a project.
///
/// Files are loaded in this order (a later file overrides an earlier one):
/// 1. `~/.airlock/airlock.<ext>`
/// 2. `~/.airlock/config.<ext>`
/// 3. `~/.airlock.<ext>`
/// 4. `<local_dir>/airlock.<ext>` (local project file)
/// 5. `<project_root>/airlock.<ext>`
/// 6. `<project_root>/airlock.local.<ext>`
///
/// If the project root is the home directory, `.airlock/airlock.<ext>` is
/// not also a user file. The user cannot see or change the local project
/// file of a sandbox in the data directory. Thus `airlock rm` removes it with
/// the sandbox.
///
/// Supported formats: TOML, JSON, YAML. For each slot, the first matching
/// extension (`toml` → `json` → `yaml` → `yml`) wins.
/// Args:
///  - `home`: Home directory of the user
///  - `project_root`: Project root directory
///  - `local_dir`: Directory of the local project file: the sandbox
///    directory for a sandbox in the data directory, else
///    `<project_root>/.airlock` (see
///    [`crate::sandboxes::local_config_dir`])
///
/// Returns:
///   The parsed files, or an error if a file exists but is not readable or
///   not valid.
pub(crate) fn discover_in(
    home: &Path,
    project_root: &Path,
    local_dir: &Path,
) -> anyhow::Result<DiscoveredFiles> {
    let local_base = local_dir.join("airlock");
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

/// Get the names of the local project files (`airlock.<ext>`) in `dir`, in
/// the order of [`EXTENSIONS`]. Only the first one is in use.
pub(crate) fn local_config_names(dir: &Path) -> Vec<String> {
    EXTENSIONS
        .iter()
        .map(|ext| format!("airlock.{ext}"))
        .filter(|name| dir.join(name).is_file())
        .collect()
}

/// Try each supported extension for `base` and parse the first file found.
/// Returns:
///   The path and parsed value, or `None` if no file exists.
pub(super) fn load_first(base: &Path) -> anyhow::Result<Option<(PathBuf, serde_json::Value)>> {
    for ext in EXTENSIONS {
        let path = PathBuf::from(format!("{}.{ext}", base.display()));
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            // Only a missing file goes to the next extension. Other errors
            // (permission, IO, a directory in place of the file) mean that
            // the config exists but is not readable. Fail closed, so that
            // the sandbox never drops the user policy without a warning and
            // uses permissive defaults.
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

/// Parse a config file. The file extension selects the format.
/// Args:
///  - `path`: File path, for the format and for error messages
///  - `content`: File contents
///
/// Returns:
///   The parsed value, or an error for a syntax error or an unsupported
///   extension.
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
