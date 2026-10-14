//! Config file discovery and parsing.
//!
//! Finds the user and project config files of a project and parses them.
//! Config files can be TOML, JSON or YAML.

use crate::util::VerifiedPath;
use crate::util::verified_path::{self, Refusal};

/// Size limit of one config file. The project files are in a directory
/// that the guest can write.
const CONFIG_CAP: u64 = 4 * 1024 * 1024;

/// Supported config file extensions, in priority order.
pub(crate) const EXTENSIONS: &[&str] = &["toml", "json", "yaml", "yml"];

/// Config files that [`discover_in`] found, each as its path (for display)
/// and parsed value. Each slot is in order of precedence, lowest first.
pub(crate) struct DiscoveredFiles {
    /// `~/.airlock/airlock.<ext>`, `~/.airlock/config.<ext>`, `~/.airlock.<ext>`
    pub user: Vec<(String, serde_json::Value)>,
    /// `<local_dir>/airlock.<ext>` (see [`discover_in`])
    pub local: Option<(String, serde_json::Value)>,
    /// `<project_root>/airlock.<ext>`, `<project_root>/airlock.local.<ext>`
    pub project: Vec<(String, serde_json::Value)>,
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
    home: &VerifiedPath,
    project_root: &VerifiedPath,
    local_dir: &VerifiedPath,
) -> anyhow::Result<DiscoveredFiles> {
    // The home dir and the project root are trusted bases. They can be
    // below a symlink. The user files can be links into a dotfiles repo.
    // The project dir and `.airlock/` are guest-writable. A file in the
    // project dir can be a link that stays in the project. A file in
    // `.airlock/` must not be a link.
    let home_dir = home;
    let project_dir = project_root;
    // A local dir in the project comes again from the project root, so no
    // link is followed below it.
    let in_project = [
        local_dir.given().strip_prefix(project_dir.given()).ok(),
        local_dir
            .to_entry_path()?
            .strip_prefix(project_dir.to_entry_path()?)
            .ok(),
    ];
    let local = match in_project.into_iter().flatten().next() {
        Some(rel) if rel.as_os_str().is_empty() => project_dir.clone(),
        Some(rel) => project_dir.join(rel)?,
        None => sandbox_local_dir(local_dir, project_dir)?,
    };
    let home_is_project = home_dir == project_dir;

    let mut user = Vec::new();
    if !home_is_project {
        user.extend(load_first(home_dir, ".airlock/airlock", Links::Any)?);
    }
    for stem in [".airlock/config", ".airlock"] {
        user.extend(load_first(home_dir, stem, Links::Any)?);
    }

    let local = load_first(&local, "airlock", Links::None)?;

    let mut project = Vec::new();
    for stem in ["airlock", "airlock.local"] {
        project.extend(load_first(project_dir, stem, Links::Within(project_dir))?);
    }

    Ok(DiscoveredFiles {
        user,
        local,
        project,
    })
}

/// Resolve the local config directory `local_dir` of a sandbox in the
/// data directory. The guest cannot write it, so it follows symlinks.
/// Args:
///  - `local_dir`: Sandbox directory in the data directory
///  - `project_dir`: Project directory
///
/// Returns:
///   The directory, or error if it resolves into the project directory.
///   A path there can have guest links, so it must come from the project
///   root.
pub(crate) fn sandbox_local_dir(
    local_dir: &VerifiedPath,
    project_dir: &VerifiedPath,
) -> anyhow::Result<VerifiedPath> {
    let dir = VerifiedPath::new_follow(local_dir.to_entry_path()?, None)?;
    if dir
        .to_entry_path()?
        .starts_with(project_dir.to_entry_path()?)
    {
        anyhow::bail!(crate::messages::config::local_dir_in_project(
            local_dir,
            project_dir
        ));
    }
    Ok(dir)
}

/// Get the names of the local project files (`airlock.<ext>`) in `dir`, in
/// the order of [`EXTENSIONS`]. Only the first one is in use.
pub(crate) fn local_config_names(dir: &VerifiedPath) -> Vec<String> {
    EXTENSIONS
        .iter()
        .map(|ext| format!("airlock.{ext}"))
        .filter(|name| {
            dir.join(name)
                .and_then(|path| path.symlink_metadata())
                .is_ok_and(|m| m.is_file())
        })
        .collect()
}

/// What a config file can be if it is a symlink.
#[derive(Clone, Copy)]
enum Links<'a> {
    /// A link to anywhere (user files).
    Any,
    /// A link that stays in the given directory.
    Within(&'a VerifiedPath),
    /// No link.
    None,
}

/// Try each supported extension for `stem` in `dir` and parse the first
/// file found.
/// Args:
///  - `dir`: Directory of the file. Its given form is the origin and
///    goes in messages.
///  - `stem`: File path below `dir` without the extension
///  - `links`: Which symlinks the file can be
///
/// Returns:
///   The path and parsed value, or `None` if no file exists.
fn load_first(
    dir: &VerifiedPath,
    stem: &str,
    links: Links,
) -> anyhow::Result<Option<(String, serde_json::Value)>> {
    for ext in EXTENSIONS {
        let name = format!("{stem}.{ext}");
        let path = dir.given().join(&name).display().to_string();
        let read = match links {
            Links::Any => dir.join_follow(&name, None),
            Links::Within(root) => dir.join_follow(&name, Some(root)),
            Links::None => dir.join(&name),
        }
        .and_then(|file| file.read_to_string_capped(CONFIG_CAP));
        let content = match read {
            Ok(content) => content,
            // Only a missing file goes to the next extension. Other errors
            // (permission, IO, a directory in place of the file, a refused
            // symlink) mean that the config exists but is not readable.
            // Fail closed, so that the sandbox never drops the user policy
            // without a warning and uses permissive defaults.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                if let Links::Within(root) = links
                    && verified_path::refusal(&e) == Some(Refusal::Outside)
                {
                    return Err(anyhow::anyhow!(
                        crate::messages::config::config_link_leaves_project(&path, root)
                    ));
                }
                return Err(anyhow::anyhow!(
                    crate::messages::config::read_config_failed(&path, e)
                ));
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
pub(crate) fn parse_file(path: &str, content: &str) -> anyhow::Result<serde_json::Value> {
    let name = path.rsplit('/').next().unwrap_or(path);
    // A leading dot starts a hidden name, not an extension.
    let ext = name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext);
    match ext {
        Some("toml") => toml::from_str(content).map_err(|e| anyhow::anyhow!("{path}: {e}")),
        Some("json") => serde_json::from_str(content).map_err(|e| anyhow::anyhow!("{path}: {e}")),
        Some("yaml" | "yml") => {
            serde_yaml::from_str(content).map_err(|e| anyhow::anyhow!("{path}: {e}"))
        }
        _ => anyhow::bail!(crate::messages::config::unsupported_config_format(path)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::test_cfg::oci::verified_dir as verified;
    use crate::test_cfg::temp_dir;

    /// Test that the config load follows user file links and project file
    /// links that stay in the project, and refuses the others. The guest
    /// can write the project dir and must not make airlock read a host
    /// file. The user keeps dotfiles in a repo and links them.
    ///   1. Link `~/.airlock/airlock.toml` to a dotfile
    ///   2. Link the project `airlock.toml` to a host file outside the
    ///      project and check that the load fails and names the file
    ///   3. Link it to a file in the project and check that it loads
    ///   4. Link `.airlock/airlock.toml` and check that the load fails
    #[test]
    fn config_links_are_followed_only_where_allowed() {
        let tmp = temp_dir();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(home.join(".airlock")).unwrap();
        std::fs::create_dir_all(project.join(".airlock")).unwrap();
        let dotfile = tmp.path().join("dotfile.toml");
        std::fs::write(&dotfile, "a = 1\n").unwrap();
        symlink(&dotfile, home.join(".airlock/airlock.toml")).unwrap();
        let host_file = tmp.path().join("host.toml");
        std::fs::write(&host_file, "b = 2\n").unwrap();
        let link = project.join("airlock.toml");
        symlink(&host_file, &link).unwrap();
        let local = project.join(".airlock");
        let (home_dir, project_dir) = (verified(&home), verified(&project));
        let local_dir = project_dir.join(".airlock").unwrap();
        let discover = || discover_in(&home_dir, &project_dir, &local_dir);

        let err = discover().err().unwrap();
        assert!(
            err.to_string().contains(&link.display().to_string()),
            "{err}"
        );
        assert!(err.to_string().contains("outside the project"), "{err}");

        std::fs::remove_file(&link).unwrap();
        std::fs::write(project.join("real.toml"), "c = 3\n").unwrap();
        symlink("real.toml", &link).unwrap();
        let files = discover().unwrap();
        assert_eq!(files.user.len(), 1);
        assert_eq!(files.user[0].1["a"], 1);
        assert_eq!(files.project.len(), 1);
        assert_eq!(files.project[0].1["c"], 3);

        symlink("../real.toml", local.join("airlock.toml")).unwrap();
        let err = discover().err().unwrap();
        let shown = local.join("airlock.toml").display().to_string();
        assert!(err.to_string().contains(&shown), "{err}");
    }

    /// Test that a data-dir local config dir that resolves into the project
    /// is refused. The guest can write the project and could plant config
    /// there.
    ///   1. Link a data-dir sandbox dir to a dir in the project
    ///   2. Check that `sandbox_local_dir` fails with the project message
    ///   3. Check that a dir outside the project is accepted
    #[test]
    fn local_dir_resolving_into_project_is_refused() {
        let tmp = temp_dir();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(tmp.path().join("data/outside")).unwrap();
        symlink(project.join("sub"), tmp.path().join("data/sandbox")).unwrap();
        let data = verified(&tmp.path().join("data"));
        let link = data.join("sandbox").unwrap();
        let project_dir = verified(&project);

        let err = sandbox_local_dir(&link, &project_dir).err().unwrap();
        assert!(
            err.to_string().contains("is in the project directory"),
            "{err}"
        );

        let ok = data.join("outside").unwrap();
        assert!(sandbox_local_dir(&ok, &project_dir).is_ok());
    }
}
