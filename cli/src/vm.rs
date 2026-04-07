#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "linux")]
mod cloud_hypervisor;
mod config;
use std::fmt::Write;
use std::os::unix::io::OwnedFd;
use std::path::Path;

use tracing::warn;

#[cfg(target_os = "linux")]
pub enum KvmStatus {
    Available,
    NotFound,
    NoPermission,
}

#[cfg(target_os = "linux")]
pub fn kvm_status() -> KvmStatus {
    use std::os::unix::fs::MetadataExt;
    let path = std::path::Path::new("/dev/kvm");
    if !path.exists() {
        return KvmStatus::NotFound;
    }
    if let Ok(metadata) = path.metadata() {
        let mode = metadata.mode();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let dev_uid = metadata.uid();
        let dev_gid = metadata.gid();

        let owner_ok = uid == dev_uid && (mode & 0o600 == 0o600);
        let group_ok = gid == dev_gid && (mode & 0o060 == 0o060);
        let other_ok = mode & 0o006 == 0o006;
        let supp_ok = if group_ok {
            false
        } else {
            let ngroups = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
            let mut groups = vec![0u32; ngroups.max(1) as usize];
            let n = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
            n > 0 && groups[..n as usize].contains(&dev_gid)
        };

        if owner_ok || group_ok || supp_ok || other_ok {
            KvmStatus::Available
        } else {
            KvmStatus::NoPermission
        }
    } else {
        KvmStatus::NoPermission
    }
}

#[cfg(target_os = "linux")]
pub fn require_kvm() {
    match kvm_status() {
        KvmStatus::Available => {}
        KvmStatus::NotFound => {
            cli::error!("KVM not available (/dev/kvm not found)");
            cli::error!("ensure KVM is enabled in your kernel/BIOS");
            std::process::exit(1);
        }
        KvmStatus::NoPermission => {
            cli::error!("no permission to access /dev/kvm");
            cli::error!("run: sudo usermod -aG kvm $USER  (then re-login)");
            std::process::exit(1);
        }
    }
}

use crate::assets::Assets;
use crate::cli;
use crate::cli::{CliArgs, LogLevel};
use crate::oci::Bundle;
use crate::project::Project;
use crate::vm::config::VmShare;

pub async fn start(
    args: &CliArgs,
    project: &Project,
    bundle: Bundle,
) -> anyhow::Result<(Box<dyn VmHandle>, OwnedFd)> {
    let assets = Assets::init()?;
    let mut shares = vec![];

    let overlay_dir = project.cache_dir.join("overlay");
    // Create overlay subdirs (rootfs is the overlayfs mount point)
    for subdir in ["rootfs", "files_rw", "files_ro"] {
        std::fs::create_dir_all(overlay_dir.join(subdir))?;
    }
    // Purge file mount dirs on each start (rebuilt from config)
    for subdir in ["files_rw", "files_ro"] {
        let dir = overlay_dir.join(subdir);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
            std::fs::create_dir_all(&dir)?;
        }
    }

    // Well-known shares
    shares.push(VmShare {
        tag: "base".to_string(),
        host_path: bundle.image_rootfs.clone(),
        read_only: true,
    });
    shares.push(VmShare {
        tag: "overlay".to_string(),
        host_path: overlay_dir.clone(),
        read_only: false,
    });

    // User dir mounts and file mounts
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
                link_file(&mount.source, &mount.target, &overlay_dir, mount.read_only)?;
            }
        }
    }

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
    let cache_paths: Vec<&str> = project
        .config
        .disk
        .cache
        .values()
        .filter(|c| c.enabled)
        .map(|c| c.path.as_str())
        .collect();
    let disk_info = if cache_paths.is_empty() {
        project.config.disk.size.to_string()
    } else {
        format!(
            "{} (caches: {})",
            project.config.disk.size,
            cache_paths.join(", ")
        )
    };
    cli::log!("  {} disk:   {}", cli::bullet(), cli::dim(&disk_info));

    let display_mounts: Vec<_> = bundle
        .mounts
        .iter()
        .filter(|m| m.display.is_some())
        .collect();
    if !display_mounts.is_empty() {
        cli::log!(
            "  {} mounts: {}",
            cli::bullet(),
            cli::dim(&display_mounts.len().to_string())
        );
        for mount in &display_mounts {
            let (source, target) = mount.display.as_ref().unwrap();
            let mode = if mount.read_only { "ro" } else { "rw" };
            cli::log!(
                "    {} {}",
                cli::bullet(),
                cli::dim(&format!("{source} → {target} ({mode})"))
            );
        }
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
                "console=hvc0 console=ttyS0 rdinit=/init ezpez.epoch={epoch} ezpez.shares={}",
                tags.join(",")
            );
            let host_ports =
                crate::network::rules::localhost_ports_from_config(&project.config.network);
            if !host_ports.is_empty() {
                let ports: Vec<String> = host_ports.iter().map(ToString::to_string).collect();
                let _ = write!(cmd, " ezpez.host_ports={}", ports.join(","));
            }
            if !matches!(args.log_level, LogLevel::Trace | LogLevel::Debug) {
                cmd.push_str(" quiet loglevel=3");
            }
            cmd
        },
        shares,
        cache_disk: Some(bundle.disk_image.clone()),
        runtime_dir: project.cache_dir.clone(),
        #[cfg(target_os = "linux")]
        cloud_hypervisor: assets.cloud_hypervisor,
        #[cfg(target_os = "linux")]
        virtiofsd: assets.virtiofsd,
        #[cfg(target_os = "linux")]
        nested_virtualization: project.config.nested_virtualization,
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

    #[cfg(target_os = "linux")]
    {
        let backend = cloud_hypervisor::CloudHypervisorBackend::start(&vm_config)?;

        let vsock_fd = {
            let mut attempts = 0u32;
            loop {
                match backend.vsock_connect() {
                    Ok(fd) => break fd,
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 60 {
                            return Err(anyhow::anyhow!(
                                "supervisor not reachable after {attempts} attempts: {e}"
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        };

        Ok((Box::new(backend), vsock_fd))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = vm_config;
        Err(anyhow::anyhow!("unsupported platform"))
    }
}

/// Link a file mount into the overlay directory, replicating the
/// target's directory structure so the entire tree can be overlaid onto
/// the container rootfs.
///
/// For target `/root/.claude.json` with read_only=false, creates
/// `overlay/files_rw/root/.claude.json`.
pub fn link_file(
    source: &Path,
    target: &str,
    overlay_dir: &Path,
    read_only: bool,
) -> anyhow::Result<()> {
    let subdir = if read_only { "files_ro" } else { "files_rw" };
    let target_path = Path::new(target);
    let parent = target_path
        .parent()
        .unwrap_or(Path::new(""))
        .strip_prefix("/")
        .unwrap_or(Path::new(""));
    let link_dir = overlay_dir.join(subdir).join(parent);
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
    Ok(())
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
impl VmHandle for cloud_hypervisor::CloudHypervisorBackend {
    fn wait_for_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
        Box::pin(cloud_hypervisor::CloudHypervisorBackend::wait_for_stop_impl(self))
    }
}
