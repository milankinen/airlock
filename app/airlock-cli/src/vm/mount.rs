//! Mount resolution.
//!
//! Converts the configured mounts to absolute host and guest paths. A mount
//! shares a directory or a single file from the host with the sandbox.

use std::path::{Path, PathBuf};

/// A mount with expanded and validated host and guest paths.
#[derive(Debug)]
pub struct ResolvedMount {
    /// Mount type: file or directory.
    pub mount_type: MountType,
    /// Expanded absolute source path on the host.
    pub source: PathBuf,
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
/// Expands `~`, makes relative paths absolute, and applies the `missing`
/// action of each mount if its source does not exist. Can create missing
/// source directories and files.
/// Args:
///  - `mounts`: Enabled mounts as `(config key, mount)` pairs, sorted by key
///  - `host_home`: Host home for `~` expansion of source paths
///  - `container_home`: Guest home for `~` expansion of target paths
///  - `cwd`: Host base directory for relative source paths
///  - `guest_cwd`: Guest base directory for relative target paths.
///
/// Returns:
///   The resolved mounts, without skipped ones. Error if a source is missing
///   and its action is `MissingAction::Fail`, if a create mode is not valid,
///   or if the creation of a source fails.
pub fn resolve_mounts(
    mounts: &[(&str, crate::config::config_values::Mount)],
    host_home: &Path,
    container_home: &str,
    cwd: &Path,
    guest_cwd: &Path,
) -> anyhow::Result<Vec<ResolvedMount>> {
    use std::os::unix::fs::PermissionsExt;

    use crate::config::config_values::MissingAction;

    let container_home = PathBuf::from(container_home);
    let mut result = Vec::new();

    let mut dir_idx: usize = 0;
    for (name, m) in mounts {
        let source = crate::util::expand_tilde(&m.source, host_home);
        // Resolve relative paths against cwd.
        let source = if source.is_relative() {
            cwd.join(&source)
        } else {
            source
        };

        // Handle a missing source.
        if !source.exists() {
            match m.missing {
                MissingAction::Fail => {
                    anyhow::bail!("mount source does not exist: {}", source.display());
                }
                MissingAction::Warn => {
                    crate::cli::log!(
                        "  {} mount skipped (not found): {}",
                        crate::cli::bullet(),
                        crate::cli::dim(&source.display().to_string())
                    );
                    continue;
                }
                MissingAction::Ignore => continue,
                MissingAction::CreateDir => {
                    std::fs::create_dir_all(&source)?;
                    let mode = parse_mode(m.create_mode.as_deref(), 0o755)?;
                    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))?;
                }
                MissingAction::CreateFile => {
                    if let Some(parent) = source.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let content = m.file_content.as_deref().unwrap_or("");
                    std::fs::write(&source, content)?;
                    let mode = parse_mode(m.create_mode.as_deref(), 0o644)?;
                    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }

        let source = std::fs::canonicalize(&source).unwrap_or(source);
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
        let mount_type = if source.is_dir() {
            let key = format!("dir_{dir_idx}");
            dir_idx += 1;
            MountType::Dir { key }
        } else {
            MountType::File {
                mount_key: name.to_string(),
            }
        };

        result.push(ResolvedMount {
            source,
            mount_type,
            target: target.to_string_lossy().to_string(),
            read_only: m.read_only,
        });
    }

    Ok(result)
}

/// Parse an octal mode string (e.g. "755") into a `u32`. Return `default`
/// if there is no string.
fn parse_mode(s: Option<&str>, default: u32) -> anyhow::Result<u32> {
    match s {
        Some(s) => {
            u32::from_str_radix(s, 8).map_err(|_| anyhow::anyhow!("invalid octal mode: {s:?}"))
        }
        None => Ok(default),
    }
}
