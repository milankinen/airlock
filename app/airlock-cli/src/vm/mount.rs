//! Mount resolution.
//!
//! Converts the configured mounts to absolute host and guest paths. A mount
//! shares a directory or a single file from the host with the sandbox.
//!
//! Mount sources can be symlinks, for example into a dotfiles repo. The
//! symlinks are resolved one time, at the start.

use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::config_values::{ConfigValues, Mount};
use crate::messages;
use crate::util::verified_path::{Refusal, refusal};
use crate::util::{VerifiedPath, vfs};

/// A mount with expanded and validated host and guest paths.
#[derive(Debug)]
pub struct ResolvedMount {
    /// Mount type: file or directory.
    pub mount_type: MountType,
    /// Config key of the mount, or `project` for the project mount.
    pub name: String,
    /// Source path as the config gives it, for example `./data`. For the
    /// project mount, the project directory.
    pub given: String,
    /// Source on the host, with its symlinks resolved at the start.
    pub source: VerifiedPath,
    /// Expanded absolute target path in the container.
    pub target: String,
    /// If `true`, the guest cannot write to the mount.
    pub read_only: bool,
}

/// Type of a mount: a directory (VirtioFS share) or a single file.
#[derive(Debug)]
pub enum MountType {
    /// A directory mount, shared to the guest as its own VirtioFS share.
    Dir {
        /// VirtioFS share tag: `project` for the project mount, and
        /// `dir_0`, `dir_1`, ... for user mounts.
        key: String,
    },
    /// A file mount. The file is hardlinked (or copied) into the sandbox
    /// overlay directory under `files/{rw|ro}/{mount_key}`. The `files/rw`
    /// and `files/ro` VirtioFS shares give these files to the guest. In the
    /// container, the target path is a symlink to
    /// `/airlock/.files/{rw|ro}/{mount_key}`.
    File {
        /// Config key of the mount.
        mount_key: String,
    },
}

impl ResolvedMount {
    /// VirtioFS share tag (for dir mounts) or config key (for file mounts).
    pub fn key(&self) -> &str {
        match &self.mount_type {
            MountType::Dir { key } => key.as_str(),
            MountType::File { mount_key } => mount_key.as_str(),
        }
    }

    /// Path of this mount in the VM, for debug output.
    pub fn vm_path(&self) -> String {
        match &self.mount_type {
            MountType::Dir { key } => format!("/mnt/{key}"),
            MountType::File { mount_key } => {
                let rw_or_ro = if self.read_only { "ro" } else { "rw" };
                format!("/airlock/.files/{rw_or_ro}/{mount_key}")
            }
        }
    }
}

/// Resolve the configured mounts to absolute paths and mount types.
///
/// Expands `~`, makes relative paths absolute, resolves symlinks, and
/// applies the `missing` action of each mount if its source does not exist.
/// Can create missing source directories and files. A create never follows
/// a symlink at the source.
/// Args:
///  - `mounts`: Enabled mounts as `(config key, mount)` pairs, sorted by key
///  - `host_home`: Host home for `~` expansion of source paths
///  - `container_home`: Guest home for `~` expansion of target paths
///  - `cwd`: Host project directory. Relative source paths resolve from it
///  - `guest_cwd`: Guest base directory for relative target paths
///
/// Returns:
///   The resolved mounts, without skipped ones. Error if a source is missing
///   and its action is `MissingAction::Fail`, if a create mode is not valid,
///   or if the creation of a source fails.
pub fn resolve_mounts(
    mounts: &[(&str, Mount)],
    host_home: &Path,
    container_home: &str,
    cwd: &Path,
    guest_cwd: &Path,
) -> anyhow::Result<Vec<ResolvedMount>> {
    use crate::config::config_values::MissingAction;

    let found = resolve_sources(mounts, host_home, cwd)?;

    let container_home = PathBuf::from(container_home);
    let mut result = Vec::new();
    let mut dir_idx: usize = 0;
    for Source {
        name,
        mount: m,
        resolved: source,
        is_dir,
    } in found
    {
        let is_dir = match is_dir {
            Some(is_dir) => is_dir,
            None => match m.missing {
                MissingAction::Fail => {
                    anyhow::bail!(messages::vm::mount_source_missing(&source));
                }
                MissingAction::Warn => {
                    crate::cli::log!(
                        "  {} {}",
                        crate::cli::bullet(),
                        messages::vm::mount_skipped_not_found(crate::cli::dim(&source.to_string()))
                    );
                    continue;
                }
                MissingAction::Ignore => continue,
                MissingAction::CreateDir => {
                    vfs::create_dir_all(&source)?;
                    let mode = parse_mode(m.create_mode.as_deref(), 0o755)?;
                    vfs::set_permissions(&source, std::fs::Permissions::from_mode(mode))?;
                    true
                }
                MissingAction::CreateFile => {
                    let mode = parse_mode(m.create_mode.as_deref(), 0o644)?;
                    create_file(&source, m.file_content.as_deref().unwrap_or(""), mode)?;
                    false
                }
            },
        };

        let target = crate::util::expand_tilde(&m.target, &container_home);
        // Resolve relative target paths against guest_cwd (the same as
        // source paths against cwd).
        let target = if target.is_relative() {
            guest_cwd.join(&target)
        } else {
            target
        };

        // Dir mounts get numbered tags (dir_0, dir_1, ...) in config key
        // order. File mounts use the config key as their identifier.
        let mount_type = if is_dir {
            let key = format!("dir_{dir_idx}");
            dir_idx += 1;
            MountType::Dir { key }
        } else {
            MountType::File {
                mount_key: name.to_string(),
            }
        };

        result.push(ResolvedMount {
            mount_type,
            name: name.to_string(),
            given: m.source.clone(),
            source,
            target: target.to_string_lossy().to_string(),
            read_only: m.read_only,
        });
    }

    Ok(result)
}

/// A mount source with its symlinks resolved. Its missing action did not
/// run yet.
struct Source<'a> {
    /// Config key of the mount.
    name: &'a str,
    /// The mount config.
    mount: &'a Mount,
    /// The path with its symlinks resolved.
    resolved: VerifiedPath,
    /// True for a directory, `None` if the source is missing.
    is_dir: Option<bool>,
}

/// Get the enabled mounts of `config`, sorted by config key.
pub(crate) fn enabled_mounts(config: &ConfigValues) -> Vec<(&str, Mount)> {
    let mut mounts: Vec<_> = config
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .map(|(k, m)| (k.as_str(), m.clone()))
        .collect();
    mounts.sort_by_key(|(k, _)| *k);
    mounts
}

/// Resolve the sources of `mounts` and follow their symlinks. Creates
/// nothing. See [`resolve_mounts`] for the args.
/// Returns:
///   The sources, or error if a source cannot be resolved.
fn resolve_sources<'a>(
    mounts: &'a [(&'a str, Mount)],
    host_home: &Path,
    cwd: &Path,
) -> anyhow::Result<Vec<Source<'a>>> {
    let mut found = Vec::new();
    for (name, m) in mounts {
        let path = absolute(&m.source, host_home, cwd);
        let source = VerifiedPath::new_follow(&path, None)?;
        let is_dir = match vfs::metadata(&source) {
            Ok(md) => Some(md.is_dir()),
            // A dangling symlink is a missing source. A create refuses it.
            Err(e) if is_missing(&e) => None,
            Err(e) => return Err(e.into()),
        };
        found.push(Source {
            name,
            mount: m,
            resolved: source,
            is_dir,
        });
    }
    Ok(found)
}

/// Make a host path from the config absolute: expand `~` from
/// `host_home`, and resolve a relative path from `cwd`.
pub(crate) fn absolute(path: &str, host_home: &Path, cwd: &Path) -> PathBuf {
    let path = crate::util::expand_tilde(path, host_home);
    if path.is_relative() {
        cwd.join(path)
    } else {
        path
    }
}

/// Check if an error of a source probe means that the source is missing.
/// A dangling symlink at the last name also counts as missing.
fn is_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || refusal(e) == Some(Refusal::Symlink)
}

/// Create the missing source file `source` and its parent directories.
fn create_file(source: &VerifiedPath, content: &str, mode: u32) -> anyhow::Result<()> {
    // The resolved path has no symlinks: its parent is the resolved parent
    // plus the missing names.
    if let Some(parent) = source.to_path()?.parent() {
        vfs::create_dir_all(&VerifiedPath::new_follow(parent, None)?)?;
    }
    // A new file never follows a symlink that appears at the name.
    let mut file = vfs::create_new(source)?;
    file.write_all(content.as_bytes())?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Parse an octal mode string (e.g. "755") into a `u32`. Return `default`
/// if there is no string.
fn parse_mode(s: Option<&str>, default: u32) -> anyhow::Result<u32> {
    match s {
        Some(s) => u32::from_str_radix(s, 8)
            .map_err(|_| anyhow::anyhow!(messages::vm::invalid_octal_mode(s))),
        None => Ok(default),
    }
}

/// Make sure that no directory mount exposes a protected host directory.
/// A mount exposes a directory if its source is the directory or one of
/// its parents, for example `/home/me`, `/home` or `/` for `/home/me`.
/// Args:
///  - `mounts`: Resolved mounts, with the project mount
///  - `protected`: `(name, path)` of each host directory that no mount can
///    expose, for example the home directory
///
/// Returns:
///   Error that names the first mount that exposes a protected directory.
pub fn check_exposure(
    mounts: &[ResolvedMount],
    protected: &[(&str, &VerifiedPath)],
) -> anyhow::Result<()> {
    // Compare the resolved paths, so that a symlink cannot hide the overlap.
    for m in mounts {
        let MountType::Dir { key } = &m.mount_type else {
            continue;
        };
        let source = m.source.to_path()?;
        let mut exposed = None;
        for (name, dir) in protected {
            if dir.to_path()?.starts_with(source) {
                exposed = Some((name, dir));
                break;
            }
        }
        let Some((name, dir)) = exposed else {
            continue;
        };
        let what = if key == "project" {
            messages::vm::MOUNT_KIND_PROJECT
        } else {
            messages::vm::MOUNT_KIND_SOURCE
        };
        anyhow::bail!(messages::vm::mount_exposes_directory(
            what, &m.source, name, dir
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory mount of `source`. `key` is `project` for the project
    /// mount.
    fn dir(key: &str, source: &Path) -> ResolvedMount {
        ResolvedMount {
            mount_type: MountType::Dir { key: key.into() },
            name: key.into(),
            given: source.display().to_string(),
            source: VerifiedPath::new_follow(source, None).unwrap(),
            target: "/mnt/x".into(),
            read_only: true,
        }
    }

    /// Test that a directory mount of the home directory, the data
    /// directory or one of their parents is refused, because the sandbox
    /// then gets all secrets in them. Mounts below them stay allowed.
    ///   1. Make a home directory with a data directory and a pack mount
    ///      directory in it
    ///   2. Check that the home, its parents (also `/`), the data directory
    ///      and a symlink to the home are refused
    ///   3. Check that a project below the home, the pack mount directory
    ///      and a file mount of the home are allowed
    #[test]
    fn mount_of_home_data_dir_or_parent_is_refused_and_subdirectory_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home/me");
        let data = home.join(".local/share/airlock");
        let pack = data.join("share/all/packs/claude");
        let project = home.join("src/app");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let home_path = VerifiedPath::new_follow(&home, None).unwrap();
        let data_path = VerifiedPath::new_follow(&data, None).unwrap();
        let protected = [
            ("home directory", &home_path),
            ("airlock data directory", &data_path),
        ];

        for (key, source) in [
            ("project", home.as_path()),
            ("project", home.parent().unwrap()),
            ("dir_0", Path::new("/")),
            ("dir_0", data.as_path()),
            ("dir_0", link.as_path()),
        ] {
            let err = check_exposure(&[dir(key, source)], &protected)
                .expect_err(&source.display().to_string())
                .to_string();
            assert!(err.contains("insecure_mounts"), "{err}");
        }

        let file = ResolvedMount {
            mount_type: MountType::File {
                mount_key: "f".into(),
            },
            ..dir("f", &home)
        };
        let allowed = [dir("project", &project), dir("dir_0", &pack), file];
        check_exposure(&allowed, &protected).unwrap();
    }
}
