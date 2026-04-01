use crate::config::Config;
use crate::project::Project;
use std::path::PathBuf;
use tracing::warn;

/// A bind mount inside the container.
pub struct ContainerBind {
    pub source: String,
    pub destination: String,
    pub read_only: bool,
}

pub(super) struct Share {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

pub struct PreparedMounts {
    pub(super) shares: Vec<Share>,
    binds: Vec<ContainerBind>,
}

impl PreparedMounts {
    pub fn binds(&self) -> &[ContainerBind] {
        &self.binds
    }
}

impl PreparedMounts {
    pub(super) fn add_share(&mut self, tag: String, host_path: PathBuf, read_only: bool) {
        self.shares.push(Share { tag, host_path, read_only });
    }
}

/// Prepare VM mounts from config and project.
///
/// Mount order (latter shadows former):
///   1. Project directory (CWD → same absolute path in container)
///   2. Config mounts in definition order
pub fn prepare(config: &Config, project: &Project) -> anyhow::Result<PreparedMounts> {
    let mut shares = Vec::new();
    let mut binds = Vec::new();

    let files_rw_dir = project.dir.join("files_rw");
    let files_ro_dir = project.dir.join("files_ro");
    if files_rw_dir.exists() { std::fs::remove_dir_all(&files_rw_dir)?; }
    if files_ro_dir.exists() { std::fs::remove_dir_all(&files_ro_dir)?; }
    let mut has_files_rw = false;
    let mut has_files_ro = false;

    // 1. Project directory
    shares.push(Share {
        tag: "project".into(),
        host_path: project.cwd.clone(),
        read_only: false,
    });
    binds.push(ContainerBind {
        source: "/mnt/project".into(),
        destination: project.cwd.to_string_lossy().into(),
        read_only: false,
    });

    // 2. CA cert for container trust store (key is passed via RPC, never on disk in VM)
    {
        std::fs::create_dir_all(&files_ro_dir)?;
        let link = files_ro_dir.join("ezpez-ca.crt");
        let _ = std::fs::hard_link(&project.ca_cert, &link)
            .or_else(|_| std::fs::copy(&project.ca_cert, &link).map(|_| ()));
        has_files_ro = true;
        binds.push(ContainerBind {
            source: "/mnt/files_ro/ezpez-ca.crt".into(),
            destination: "/usr/local/share/ca-certificates/ezpez-ca.crt".into(),
            read_only: true,
        });
    }

    // 3. Config mounts
    for (i, mount) in config.mounts.iter().enumerate() {
        let source = PathBuf::from(&mount.source);
        let source = std::fs::canonicalize(&source).unwrap_or(source);

        if source.is_file() {
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
            shares.push(Share { tag: tag.clone(), host_path: source, read_only: mount.read_only });
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
        shares.push(Share { tag: "files_rw".into(), host_path: files_rw_dir, read_only: false });
    }
    if has_files_ro {
        shares.push(Share { tag: "files_ro".into(), host_path: files_ro_dir, read_only: true });
    }

    Ok(PreparedMounts { shares, binds })
}
