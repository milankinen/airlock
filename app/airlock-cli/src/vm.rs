//! VM lifecycle: configure, boot, and connect to the in-VM supervisor.
//!
//! On macOS, uses the Apple Virtualization.framework. On Linux, uses
//! cloud-hypervisor + virtiofsd.

#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "linux")]
mod cloud_hypervisor;
mod config;
pub(crate) mod disk;
mod file_sync;
pub mod mount;

use std::os::unix::io::OwnedFd;
use std::path::{Path, PathBuf};

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
use crate::oci::OciImage;
use crate::project::Project;
use crate::vm::config::VmShare;

/// A running VM instance. Dropping this kills the VM and stops file sync.
#[allow(dead_code)]
pub struct VmInstance {
    /// Private — dropping kills the VM via the existing backend impls.
    vm_handle: Box<dyn VmHandle>,
    /// File-sync handle — gracefully drained by `shutdown()`, aborted on drop.
    sync_handle: Option<file_sync::SyncHandle>,
    pub image_id: String,
    pub image_layers: Vec<String>,
    pub mounts: Vec<mount::ResolvedMount>,
    pub disk_image: PathBuf,
    pub caches: Vec<disk::CacheEntry>,
    pub container_home: String,
    /// Fully resolved command (args.args + login shell applied).
    pub cmd: Vec<String>,
    /// Fully resolved environment (project.config.env overrides applied).
    pub env: Vec<String>,
    pub cwd: String,
    pub uid: u32,
    pub gid: u32,
}

impl VmInstance {
    /// Gracefully shut down file sync (drains pending events) then drop the VM.
    pub async fn shutdown(mut self) {
        if let Some(handle) = self.sync_handle.take() {
            handle.shutdown().await;
        }
    }
}

/// Boot the VM with the given config and image. Returns a `VmInstance` (for
/// cleanup on drop) and the vsock fd connected to the in-VM supervisor.
pub async fn start(
    args: &CliArgs,
    project: &Project,
    image: &OciImage,
) -> anyhow::Result<(VmInstance, OwnedFd)> {
    let assets = Assets::init(project)?;
    // CA overlay lives at sandbox_dir/ca/; files overlay at sandbox_dir/overlay/
    let overlay_dir = project.sandbox_dir.join("overlay");

    project.install_ca_cert(&image.rootfs)?;

    let mounts = assemble_mounts(project, image)?;
    let shares = prepare_shares(image, &mounts, &project.sandbox_dir)?;
    let (disk_image, caches) = disk::prepare(
        &project.sandbox_dir,
        &project.config.disk,
        &image.container_home,
        &project.host_cwd,
    )?;
    let cmd = resolve_cmd(args, image);
    let env = resolve_env(project, image)?;
    let cwd = project.guest_cwd.to_string_lossy().into_owned();

    log_config(project, &shares);

    let vm_config = config::VmConfig {
        cpus: project.config.vm.cpus,
        memory_bytes: project.config.vm.memory.0,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        kernel_cmdline: build_kernel_cmdline(args),
        shares,
        cache_disk: Some(disk_image.clone()),
        runtime_dir: project.sandbox_dir.clone(),
        #[cfg(target_os = "linux")]
        cloud_hypervisor: assets.cloud_hypervisor,
        #[cfg(target_os = "linux")]
        virtiofsd: assets.virtiofsd,
        #[cfg(target_os = "linux")]
        kvm: project.config.vm.kvm,
    };

    let (vm_handle, vsock_fd) = boot_backend(&vm_config).await?;
    let sync_handle = file_sync::start(&mounts, &overlay_dir);

    Ok((
        VmInstance {
            vm_handle,
            sync_handle,
            image_id: image.image_id.clone(),
            image_layers: image.image_layers.clone(),
            mounts,
            disk_image,
            caches,
            container_home: image.container_home.clone(),
            cmd,
            env,
            cwd,
            uid: image.uid,
            gid: image.gid,
        },
        vsock_fd,
    ))
}

/// Build the sandbox dir mount and resolve all enabled user mounts.
fn assemble_mounts(
    project: &Project,
    image: &OciImage,
) -> anyhow::Result<Vec<mount::ResolvedMount>> {
    let project_mount = mount::ResolvedMount {
        mount_type: mount::MountType::Dir {
            key: "project".to_string(),
        },
        source: project.host_cwd.clone(),
        target: project.guest_cwd.to_string_lossy().into(),
        read_only: false,
    };

    let mut enabled_mounts: Vec<_> = project
        .config
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .map(|(k, m)| (k.as_str(), m.clone()))
        .collect();
    enabled_mounts.sort_by_key(|(k, _)| *k);
    let user_mounts = mount::resolve_mounts(
        &enabled_mounts,
        &project.host_home,
        &image.container_home,
        &project.host_cwd,
        &project.guest_cwd,
    )?;

    let mut mounts = vec![project_mount];
    mounts.extend(user_mounts);
    Ok(mounts)
}

/// Build the VirtioFS share list from static shares + dir mounts + file mounts.
///
/// File mounts are hard-linked (copy fallback on EXDEV) into
/// `overlay/files/{rw,ro}/{key}` and exposed as two consolidated shares.
/// CA overlay lives at `sandbox_dir/ca/`; files overlay at `sandbox_dir/overlay/files/`.
fn prepare_shares(
    _image: &OciImage,
    mounts: &[mount::ResolvedMount],
    sandbox_dir: &Path,
) -> anyhow::Result<Vec<VmShare>> {
    // The guest composes the image rootfs via overlayfs from `/mnt/layers/<d>/rootfs`.
    // Share the shared per-layer cache root once; the guest reads only the
    // digests listed in `imageLayers` for this image.
    let mut shares = vec![VmShare {
        tag: "layers".to_string(),
        host_path: crate::cache::layers_root()?,
        read_only: true,
    }];
    // CA overlay is at sandbox_dir/ca/ (was overlay/ca/)
    let ca_dir = sandbox_dir.join("ca");
    if ca_dir.exists() {
        shares.push(VmShare {
            tag: "ca".to_string(),
            host_path: ca_dir,
            read_only: true,
        });
    }

    for m in mounts
        .iter()
        .filter(|m| matches!(m.mount_type, mount::MountType::Dir { .. }))
    {
        tracing::debug!(
            "mount: {} → {} → {} (read-only: {})",
            m.source.display(),
            m.vm_path(),
            m.target,
            m.read_only
        );
        shares.push(VmShare {
            tag: m.key().into(),
            host_path: m.source.clone(),
            read_only: m.read_only,
        });
    }

    // Hard-link file mounts into overlay/files/{rw|ro}/{key}. Rebuild from
    // scratch each boot so stale entries are removed.
    let files_rw_dir = sandbox_dir.join("overlay").join("files").join("rw");
    let files_ro_dir = sandbox_dir.join("overlay").join("files").join("ro");
    let _ = std::fs::remove_dir_all(&files_rw_dir);
    let _ = std::fs::remove_dir_all(&files_ro_dir);
    let mut has_rw_files = false;
    let mut has_ro_files = false;

    for m in mounts
        .iter()
        .filter(|m| matches!(m.mount_type, mount::MountType::File { .. }))
    {
        tracing::debug!(
            "file mount: {} → {} (read-only: {})",
            m.source.display(),
            m.target,
            m.read_only
        );
        let dir = if m.read_only {
            &files_ro_dir
        } else {
            &files_rw_dir
        };
        std::fs::create_dir_all(dir)?;
        let link_path = dir.join(m.key());
        if let Err(e) = std::fs::hard_link(&m.source, &link_path) {
            if e.kind() == std::io::ErrorKind::CrossesDevices {
                cli::log!(
                    "file mount {}: cross-device hard link failed, falling back to copy \
                     (writes inside the VM will NOT sync back to the host)",
                    m.source.display()
                );
                std::fs::copy(&m.source, &link_path)?;
            } else {
                return Err(
                    anyhow::Error::from(e).context(format!("file mount {}", m.source.display()))
                );
            }
        }
        has_ro_files = has_ro_files || m.read_only;
        has_rw_files = has_rw_files || !m.read_only;
    }

    if has_rw_files {
        shares.push(VmShare {
            tag: "files/rw".to_string(),
            host_path: files_rw_dir,
            read_only: false,
        });
    }
    if has_ro_files {
        shares.push(VmShare {
            tag: "files/ro".to_string(),
            host_path: files_ro_dir,
            read_only: true,
        });
    }

    Ok(shares)
}

/// Resolve the final container command: args override, then login shell wrap.
fn resolve_cmd(args: &CliArgs, image: &OciImage) -> Vec<String> {
    let cmd = if args.args.is_empty() {
        image.cmd.clone()
    } else {
        args.args.clone()
    };
    if args.login {
        crate::oci::apply_login_shell(cmd)
    } else {
        cmd
    }
}

/// Resolve the final container environment: image env with project overrides applied.
fn resolve_env(project: &Project, image: &OciImage) -> anyhow::Result<Vec<String>> {
    let mut env = image.env.clone();
    for (key, template) in &project.config.env {
        let value = project
            .vault
            .subst(template)
            .map_err(|e| anyhow::anyhow!("env.{key}: {e}"))?;
        env.retain(|existing| !existing.starts_with(&format!("{key}=")));
        env.push(format!("{key}={value}"));
    }
    Ok(env)
}

/// Build the kernel command line string.
fn build_kernel_cmdline(args: &CliArgs) -> String {
    let mut cmdline = "console=hvc0 console=ttyS0 rdinit=/init".to_string();
    if !matches!(args.log_level, LogLevel::Trace | LogLevel::Debug) {
        cmdline.push_str(" quiet loglevel=3");
    }
    cmdline
}

/// Log the resolved VM configuration to the terminal and trace output.
fn log_config(project: &Project, shares: &[VmShare]) {
    cli::log!(
        "  {} cpus:   {}",
        cli::bullet(),
        cli::dim(&project.config.vm.cpus.to_string())
    );
    cli::log!(
        "  {} memory: {}",
        cli::bullet(),
        cli::dim(&project.config.vm.memory.to_string())
    );
    cli::log!(
        "  {} disk:   {}",
        cli::bullet(),
        cli::dim(&project.config.disk.size.to_string())
    );
    for share in shares {
        tracing::debug!(
            "share: tag={}, host_path={}, ro={}",
            share.tag,
            share.host_path.display(),
            share.read_only
        );
    }
}

/// Start the platform-specific VM backend and wait for the supervisor vsock.
async fn boot_backend(
    vm_config: &config::VmConfig,
) -> anyhow::Result<(Box<dyn VmHandle>, OwnedFd)> {
    #[cfg(target_os = "macos")]
    {
        let mut backend = apple::AppleVmBackend::new(vm_config)?;
        backend.start().await?;
        let vsock_fd = {
            let mut attempts = 0;
            loop {
                match backend.vsock_connect(airlock_common::SUPERVISOR_PORT).await {
                    Ok(fd) => break fd,
                    Err(e) => {
                        attempts += 1;
                        if attempts >= 30 {
                            return Err(anyhow::anyhow!(
                                "supervisor not reachable after {attempts} attempts: {e}"
                            ));
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
        let backend = cloud_hypervisor::CloudHypervisorBackend::start(vm_config)?;
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

/// Trait for VM backends. Dropping the handle kills the VM.
#[allow(dead_code)]
trait VmHandle {
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
