//! Mount resolution: expand paths, classify dir vs file mounts.

use std::path::{Path, PathBuf};

/// A mount with host/guest paths fully expanded and validated.
#[derive(Debug)]
pub struct ResolvedMount {
    /// Mount type: file / directory
    pub mount_type: MountType,
    /// Expanded absolute source path on host.
    pub source: PathBuf,
    /// Expanded absolute target path in container.
    pub target: String,
    pub read_only: bool,
}

/// Whether a mount is a directory (VirtioFS share) or a single file.
#[derive(Debug)]
pub enum MountType {
    Dir {
        key: String,
    },
    /// File mounts are hard-linked (with copy fallback) into the project
    /// overlay directory under `files/{rw|ro}/{mount_key}`, and exposed via
    /// `files/rw` / `files/ro` VirtioFS shares. Inside the container, the
    /// target path becomes a symlink → `/airlock/.files/{rw|ro}/{mount_key}`.
    File {
        mount_key: String,
    },
}

impl ResolvedMount {
    /// VirtioFS share tag (for Dir mounts) or config key (for File mounts).
    pub fn key(&self) -> &str {
        match &self.mount_type {
            MountType::Dir { key } => key.as_str(),
            MountType::File { mount_key } => mount_key.as_str(),
        }
    }

    /// Debug path: where this mount is accessible in the VM environment.
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

/// Expand `~` in mount paths, handle missing sources, and classify as
/// dir or file mounts.
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
        // Resolve relative paths against cwd
        let source = if source.is_relative() {
            cwd.join(&source)
        } else {
            source
        };

        // Handle missing source
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
        // Resolve relative target paths against guest_cwd (mirrors source → cwd behavior)
        let target = if target.is_relative() {
            guest_cwd.join(&target)
        } else {
            target
        };

        // Dir mounts get indexed tags (dir_0, dir_1, …) sorted by config key.
        // File mounts use the config key as their identifier.
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

/// Parse an octal mode string (e.g. "755") into a `u32`, or return the default.
fn parse_mode(s: Option<&str>, default: u32) -> anyhow::Result<u32> {
    match s {
        Some(s) => {
            u32::from_str_radix(s, 8).map_err(|_| anyhow::anyhow!("invalid octal mode: {s:?}"))
        }
        None => Ok(default),
    }
}
