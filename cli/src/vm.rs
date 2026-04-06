#[cfg(target_os = "macos")]
mod apple;
mod config;
#[cfg(target_os = "linux")]
mod krun;
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use std::fmt::Write;
use std::os::unix::io::OwnedFd;
use std::path::Path;

#[cfg(target_os = "linux")]
pub use krun::check_kvm_access;
use tracing::warn;

use crate::assets::Assets;
use crate::cli;
use crate::cli::CliArgs;
#[cfg(target_os = "macos")]
use crate::cli::LogLevel;
use crate::oci::Bundle;
use crate::project::Project;
use crate::vm::config::VmShare;

pub async fn start(
    #[cfg_attr(target_os = "linux", allow(unused_variables))] args: &CliArgs,
    project: &Project,
    bundle: Bundle,
) -> anyhow::Result<(Box<dyn VmHandle>, OwnedFd, Vec<String>)> {
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
                let share = link_file(mount.key(), &mount.source, &mount.target, &files_dir)?;
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

    let share_tags: Vec<String> = shares.iter().map(|s| s.tag.clone()).collect();

    let vm_config = config::VmConfig {
        cpus: project.config.cpus,
        memory_bytes: project.config.memory.0,
        #[cfg(target_os = "macos")]
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        #[cfg(target_os = "macos")]
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
        runtime_dir: project.cache_dir.clone(),
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

        Ok((Box::new(backend), vsock_fd, share_tags.clone()))
    }

    #[cfg(target_os = "linux")]
    {
        let backend = krun::KrunVmBackend::start(&vm_config, &assets.libkrun, &assets.libkrunfw)?;

        // Wait for the VM to boot and supervisor to start listening.
        // With libkrun, the first successful connect may reach the guest
        // before the supervisor binds its vsock port, causing an RST.
        // We retry the full connect to handle this race.
        let vsock_fd = {
            let mut attempts = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                match backend.vsock_connect() {
                    Ok(fd) => {
                        // Verify the connection is alive by peeking
                        use std::os::unix::io::AsRawFd;
                        let mut buf = [0u8; 1];
                        let ret = unsafe {
                            libc::recv(
                                fd.as_raw_fd(),
                                buf.as_mut_ptr().cast(),
                                1,
                                libc::MSG_PEEK | libc::MSG_DONTWAIT,
                            )
                        };
                        if ret == 0
                            || (ret < 0
                                && std::io::Error::last_os_error().raw_os_error()
                                    == Some(libc::ECONNRESET))
                        {
                            // Connection was RST — supervisor not ready yet
                            attempts += 1;
                            if attempts >= 60 {
                                return Err(anyhow::anyhow!(
                                    "supervisor not reachable after {attempts} attempts"
                                ));
                            }
                            continue;
                        }
                        break fd;
                    }
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 60 {
                            return Err(anyhow::anyhow!(format!(
                                "supervisor not reachable after {attempts} attempts: {e}"
                            )));
                        }
                    }
                }
            }
        };

        Ok((Box::new(backend), vsock_fd, share_tags.clone()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = vm_config;
        Err(anyhow::anyhow!("unsupported platform"))
    }
}

/// Link a file mount into the shared files directory, replicating the
/// target's directory structure so the entire tree can be overlaid onto
/// the container rootfs.
///
/// For target `/root/.claude.json`, creates `files_rw/root/.claude.json`.
pub fn link_file(
    ftag: &str,
    source: &Path,
    target: &str,
    files_dir: &Path,
) -> anyhow::Result<VmShare> {
    let fdir = files_dir.join(ftag);
    // Replicate target directory structure: /root/.claude.json → files_rw/root/
    let target_path = Path::new(target);
    let parent = target_path
        .parent()
        .unwrap_or(Path::new(""))
        .strip_prefix("/")
        .unwrap_or(Path::new(""));
    let link_dir = fdir.join(parent);
    std::fs::create_dir_all(&link_dir)?;

    let file_name = target_path
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().to_string());
    let link_path = link_dir.join(&file_name);

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
        read_only: false, // overlay upperdir must be writable
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

#[cfg(target_os = "linux")]
impl VmHandle for krun::KrunVmBackend {
    fn wait_for_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
        Box::pin(krun::KrunVmBackend::wait_for_stop_impl(self))
    }
}
