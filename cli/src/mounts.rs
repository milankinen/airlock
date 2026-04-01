use crate::config::Config;
use std::path::{Path, PathBuf};
use tracing::warn;

/// A prepared share to be mounted via VirtioFS into the VM.
pub struct VirtioShare {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

/// A bind mount inside the container (from VirtioFS mount to container path).
pub struct ContainerBind {
    pub source: String,    // path inside VM, e.g. /mnt/project
    pub destination: String, // path inside container
    pub read_only: bool,
}

/// Prepared mounts for a session.
pub struct PreparedMounts {
    pub shares: Vec<VirtioShare>,
    pub binds: Vec<ContainerBind>,
}

/// Prepare all mounts for the VM and container.
///
/// Creates VirtioFS shares and container bind mount entries.
/// For file mounts, hard-links files into the project's files/ directory.
///
/// Mount order (latter shadows former):
///   1. Project directory (CWD, mounted at the same path as host)
///   2. Config mounts in order
pub fn prepare(config: &Config, project_dir: &Path) -> anyhow::Result<PreparedMounts> {
    let mut shares = Vec::new();
    let mut binds = Vec::new();

    // Prepare files directories for file mounts (reset each run)
    // Separate dirs for rw/ro since VirtioFS shares are rw or ro at share level
    let files_rw_dir = project_dir.join("files_rw");
    let files_ro_dir = project_dir.join("files_ro");
    if files_rw_dir.exists() { std::fs::remove_dir_all(&files_rw_dir)?; }
    if files_ro_dir.exists() { std::fs::remove_dir_all(&files_ro_dir)?; }
    let mut has_files_rw = false;
    let mut has_files_ro = false;

    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    // 1. Project directory — mounted at the same absolute path as on host
    shares.push(VirtioShare {
        tag: "project".into(),
        host_path: cwd.clone(),
        read_only: false,
    });
    binds.push(ContainerBind {
        source: "/mnt/project".into(),
        destination: cwd.to_string_lossy().into(),
        read_only: false,
    });

    // 2. Config mounts in order
    for (i, mount) in config.mounts.iter().enumerate() {
        let source = PathBuf::from(&mount.source);
        let source = std::fs::canonicalize(&source).unwrap_or(source);

        if source.is_file() {
            // File mount — hard-link into appropriate files directory
            let (fdir, ftag) = if mount.read_only {
                has_files_ro = true;
                (&files_ro_dir, "files_ro")
            } else {
                has_files_rw = true;
                (&files_rw_dir, "files_rw")
            };
            std::fs::create_dir_all(fdir)?;

            let file_name = format!("f{i}_{}", source.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("file_{i}")));
            let link_path = fdir.join(&file_name);

            if let Err(e) = std::fs::hard_link(&source, &link_path) {
                warn!("cannot hard-link {}: {e} (file will not be synced)", source.display());
                if let Err(e2) = std::fs::copy(&source, &link_path) {
                    warn!("cannot copy {}: {e2}, skipping mount", source.display());
                    continue;
                }
            }

            binds.push(ContainerBind {
                source: format!("/mnt/{ftag}/{file_name}"),
                destination: mount.target.clone(),
                read_only: mount.read_only,
            });
        } else if source.is_dir() {
            let tag = format!("mount_{i}");
            shares.push(VirtioShare {
                tag: tag.clone(),
                host_path: source,
                read_only: mount.read_only,
            });
            binds.push(ContainerBind {
                source: format!("/mnt/{tag}"),
                destination: mount.target.clone(),
                read_only: mount.read_only,
            });
        } else {
            warn!("mount source does not exist: {}", mount.source);
        }
    }

    if has_files_rw {
        shares.push(VirtioShare {
            tag: "files_rw".into(),
            host_path: files_rw_dir,
            read_only: false,
        });
    }
    if has_files_ro {
        shares.push(VirtioShare {
            tag: "files_ro".into(),
            host_path: files_ro_dir,
            read_only: true,
        });
    }

    Ok(PreparedMounts { shares, binds })
}
