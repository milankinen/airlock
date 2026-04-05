#[cfg(target_os = "macos")]
mod apple;
mod config;

use std::collections::HashSet;
use std::fmt::Write;
use std::os::unix::io::OwnedFd;
use std::path::Path;

use tracing::warn;

use crate::assets::Assets;
use crate::cli;
use crate::cli::{CliArgs, LogLevel};
use crate::oci::Bundle;
use crate::project::Project;
use crate::vm::config::VmShare;

#[allow(clippy::unused_async)]
pub async fn start(
    args: &CliArgs,
    project: &Project,
    bundle: Bundle,
) -> anyhow::Result<(Box<dyn VmHandle>, OwnedFd)> {
    let assets = Assets::init()?;
    let mut shares = vec![];
    let mut file_share_tags = HashSet::new();

    let files_dir = project.cache_dir.join("files");
    // Purge existing file mounts
    if files_dir.exists() {
        std::fs::remove_dir_all(&files_dir)?;
    }

    // Add user config mounts from resolved bundle
    for mount in &bundle.mounts {
        tracing::debug!(
            "mount: {} → {} → {} (read-only: {})",
            mount.source.display(),
            mount.vm_path(),
            mount.target,
            mount.read_only
        );
        match &mount.mount_type {
            crate::oci::MountType::Dir { .. } => {
                shares.push(VmShare {
                    tag: mount.key().into(),
                    host_path: mount.source.clone(),
                    read_only: mount.read_only,
                });
            }
            crate::oci::MountType::File { .. } => {
                let share = link_file(mount.key(), &mount.source, mount.read_only, &files_dir)?;
                if !file_share_tags.contains(&share.tag) {
                    file_share_tags.insert(share.tag.clone());
                    shares.push(share);
                }
            }
            crate::oci::MountType::Cache { .. } => {
                // Cache mounts use VirtIO block device, not VirtioFS
            }
        }
    }
    // Add bundle as a VirtioFS share
    shares.push(VmShare {
        tag: "bundle".to_string(),
        host_path: bundle.path.clone(),
        read_only: false,
    });

    cli::log!(
        "  {} cpus:   {}",
        cli::bullet(),
        cli::dim(&project.config.cpus.to_string())
    );
    cli::log!(
        "  {} memory: {}",
        cli::bullet(),
        cli::dim(&project.config.memory.to_string())
    );
    cli::log!(
        "  {} cache:  {}",
        cli::bullet(),
        cli::dim(
            &project
                .config
                .cache
                .as_ref()
                .map_or_else(|| "none".to_string(), |c| c.size.to_string())
        )
    );
    for mount in &bundle.mounts {
        let Some((source, target)) = &mount.display else {
            continue;
        };
        let mode = if matches!(mount.mount_type, crate::oci::MountType::Cache { .. }) {
            continue;
        } else if mount.read_only {
            "ro"
        } else {
            "rw"
        };
        cli::log!(
            "  {} mount:  {}",
            cli::bullet(),
            cli::dim(&format!("{source} → {target} ({mode})"))
        );
    }

    for share in &shares {
        tracing::debug!(
            "share: tag={}, host_path={}, ro={}",
            share.tag,
            share.host_path.display(),
            share.read_only
        );
    }

    let vm_config = config::VmConfig {
        cpus: project.config.cpus,
        memory_bytes: project.config.memory.0,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        kernel_cmdline: {
            let tags: Vec<&str> = shares.iter().map(|s| s.tag.as_str()).collect();
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut cmd = format!(
                "console=hvc0 rdinit=/init ezpez.epoch={epoch} ezpez.shares={}",
                tags.join(",")
            );
            if !project.config.network.host_ports.is_empty() {
                let ports: Vec<String> = project
                    .config
                    .network
                    .host_ports
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let _ = write!(cmd, " ezpez.host_ports={}", ports.join(","));
            }
            if !matches!(args.log_level, LogLevel::Trace | LogLevel::Debug) {
                cmd.push_str(" quiet loglevel=3");
            }
            cmd
        },
        shares,
        cache_disk: bundle.cache_image.clone(),
    };

    #[cfg(target_os = "macos")]
    {
        let mut backend = apple::AppleVmBackend::new(&vm_config)?;
        backend.start().await?;

        let vsock_fd = {
            let mut attempts = 0;
            loop {
                match backend.vsock_connect(ezpez_protocol::SUPERVISOR_PORT).await {
                    Ok(fd) => break fd,
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 30 {
                            return Err(anyhow::anyhow!(format!(
                                "supervisor not reachable after {attempts} attempts: {e}"
                            )));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        };

        Ok((Box::new(backend), vsock_fd))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = vm_config;
        Err(anyhow::anyhow!("only macOS is supported currently"))
    }
}

pub fn link_file(
    ftag: &str,
    source: &Path,
    read_only: bool,
    files_dir: &Path,
) -> anyhow::Result<VmShare> {
    let fdir = files_dir.join(ftag);
    std::fs::create_dir_all(&fdir)?;

    let file_name = source
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().to_string());
    let link_path = fdir.join(&file_name);

    if let Err(e) = std::fs::hard_link(source, &link_path) {
        warn!(
            "cannot hard-link {}: {e} (file will not be synced)",
            source.display()
        );
        if let Err(e2) = std::fs::copy(source, &link_path) {
            anyhow::bail!("cannot copy {}: {e2}", source.display());
        }
    }
    tracing::debug!(
        "hard linked shared file: {} → {}",
        source.display(),
        link_path.display()
    );
    Ok(VmShare {
        tag: ftag.into(),
        host_path: fdir,
        read_only,
    })
}

#[allow(dead_code)]
pub trait VmHandle {
    fn wait_for_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>;
}

#[cfg(target_os = "macos")]
impl VmHandle for apple::AppleVmBackend {
    fn wait_for_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
        Box::pin(apple::AppleVmBackend::wait_for_stop_impl(self))
    }
}
