//! VM lifecycle support.
//!
//! Configures, boots and controls the sandbox VM, and connects to the
//! supervisor in the VM. Also resolves the configured mounts and manages the
//! persistent sandbox disk. On Linux, it checks that the user can use KVM.
//!
//! On macOS, the VM uses the Apple Virtualization framework. On Linux, it
//! uses Cloud Hypervisor and virtiofsd.

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

/// Result of the KVM access check.
#[cfg(target_os = "linux")]
pub enum KvmStatus {
    /// `/dev/kvm` can be opened for read and write.
    Available,
    /// `/dev/kvm` does not exist.
    NotFound,
    /// The user has no permission to open `/dev/kvm`.
    NoPermission,
    /// `/dev/kvm` cannot be opened for a different reason.
    Unavailable(std::io::Error),
}

/// Check if the current user can use `/dev/kvm`.
#[cfg(target_os = "linux")]
pub fn kvm_status() -> KvmStatus {
    kvm_status_at(Path::new("/dev/kvm"))
}

/// Check if the current user can open the KVM device at `path`.
#[cfg(target_os = "linux")]
fn kvm_status_at(path: &Path) -> KvmStatus {
    // Open the file instead of reading the mode bits, so the kernel also
    // applies ACLs.
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(_) => KvmStatus::Available,
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => KvmStatus::NotFound,
            std::io::ErrorKind::PermissionDenied => KvmStatus::NoPermission,
            _ => KvmStatus::Unavailable(err),
        },
    }
}

/// Make sure that KVM is available. If not, show the reason and exit the
/// process.
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
        KvmStatus::Unavailable(err) => {
            cli::error!("cannot open /dev/kvm: {err}");
            std::process::exit(1);
        }
    }
}

use crate::assets::Assets;
use crate::cli;
use crate::cli::LogLevel;
use crate::oci::OciImage;
use crate::project::Project;
use crate::sandbox::boot::BootOptions;
use crate::vm::config::VmShare;

/// A running VM instance. When dropped, it kills the VM and stops the file
/// sync.
#[allow(dead_code)]
pub struct VmInstance {
    /// Backend handle. When dropped, the backend kills the VM.
    vm_handle: Box<dyn VmHandle>,
    /// File-sync handle. `shutdown()` stops it cleanly. A drop aborts it.
    sync_handle: Option<file_sync::SyncHandle>,
    /// OCI image digest.
    pub image_id: String,
    /// Image layer keys, topmost first.
    pub image_layers: Vec<String>,
    /// Resolved mounts, including the project mount if it is shared.
    pub mounts: Vec<mount::ResolvedMount>,
    /// Path of the sandbox disk image.
    pub disk_image: PathBuf,
    /// Cache directories on the sandbox disk.
    pub caches: Vec<disk::CacheEntry>,
    /// Resolved guest home directory.
    pub container_home: String,
    /// The sandbox env (see [`crate::sandbox::boot::guest_env`]).
    pub env: Vec<String>,
    /// Working directory in the guest.
    pub cwd: String,
    /// Container uid.
    pub uid: u32,
    /// Container gid.
    pub gid: u32,
}

impl VmInstance {
    /// Stop the file sync cleanly (it handles pending events), then stop the
    /// VM and wait until the backend confirms the stop.
    /// Returns:
    ///   Error if the stop was not confirmed. The VM is still killed when
    ///   `self` is dropped.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        if let Some(handle) = self.sync_handle.take() {
            handle.shutdown().await;
        }
        self.vm_handle.stop().await
    }

    /// Open a vsock connection to the given guest port. Tries again every
    /// 200 ms for up to approximately 12 s.
    /// Returns:
    ///   The connected socket, or error if all attempts fail.
    pub async fn vsock_connect(&self, port: u32) -> anyhow::Result<OwnedFd> {
        // Retries are necessary because the guest can still be in boot
        // (first call), or can have only just started to listen on a new
        // port after the initial handshake (later calls).
        const MAX_ATTEMPTS: u32 = 60;
        const DELAY: std::time::Duration = std::time::Duration::from_millis(200);
        let mut last_err = None;
        for _ in 0..MAX_ATTEMPTS {
            match self.vm_handle.vsock_connect(port).await {
                Ok(fd) => return Ok(fd),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(DELAY).await;
                }
            }
        }
        Err(anyhow::anyhow!(
            "vsock connect to port {port} failed after {MAX_ATTEMPTS} attempts: {}",
            last_err.expect("at least one attempt")
        ))
    }
}

/// Boot the sandbox VM and connect to the in-VM supervisor.
/// Args:
///  - `project`: The project with its config
///  - `image`: The prepared OCI image
///  - `container_home`: Resolved guest home, see
///    [`crate::oci::effective_container_home`]. Used for `~/...` expansion
///    of mount, cache and socket-forward paths
///  - `env`: The sandbox env ([`crate::sandbox::boot::guest_env`])
///  - `opts`: Boot options. If `project_share` is false, there is no project
///    mount and the guest runs in `/`. `quiet` skips the VM resources summary.
///
/// Returns:
///   The [`VmInstance`] (it cleans up on drop) and the vsock fd that is
///   connected to the in-VM supervisor.
pub async fn start(
    project: &Project,
    image: &OciImage,
    container_home: &str,
    env: Vec<String>,
    opts: &BootOptions,
) -> anyhow::Result<(VmInstance, OwnedFd)> {
    let assets = Assets::init(project)?;
    let overlay_dir = project.sandbox_dir.join("overlay");

    let mounts = assemble_mounts(project, container_home, opts.project_share)?;
    let shares = prepare_shares(image, &mounts, &project.sandbox_dir)?;
    let (disk_image, caches) = disk::prepare(
        &project.sandbox_dir,
        &project.config.disk,
        container_home,
        &project.host_cwd,
    )?;
    let cwd = if opts.project_share {
        project.guest_cwd.to_string_lossy().into_owned()
    } else {
        "/".to_string()
    };

    if !opts.quiet {
        log_config(project);
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
        cpus: project.config.vm.cpus,
        memory_bytes: project.config.vm.memory.0,
        kernel: assets.kernel,
        initramfs: assets.initramfs,
        kernel_cmdline: build_kernel_cmdline(opts.log_level),
        shares,
        cache_disk: Some(disk_image.clone()),
        runtime_dir: project.sandbox_dir.clone(),
        #[cfg(target_os = "linux")]
        cloud_hypervisor: assets.cloud_hypervisor,
        #[cfg(target_os = "linux")]
        virtiofsd: assets.virtiofsd,
        kvm: project.config.vm.kvm,
    };

    let vm_handle = boot_backend(&vm_config).await?;
    let sync_handle = file_sync::start(&mounts, &overlay_dir);

    let vm = VmInstance {
        vm_handle,
        sync_handle,
        image_id: image.image_id.clone(),
        image_layers: image.image_layers.clone(),
        mounts,
        disk_image,
        caches,
        container_home: container_home.to_string(),
        env,
        cwd,
        uid: image.uid,
        gid: image.gid,
    };
    // Wait until the in-VM supervisor listens. All later vsock connections
    // use the same retry loop.
    let vsock_fd = vm.vsock_connect(airlock_common::SUPERVISOR_PORT).await?;
    Ok((vm, vsock_fd))
}

/// Make the project dir mount (if `project_share` is true) and resolve all
/// enabled user mounts.
fn assemble_mounts(
    project: &Project,
    container_home: &str,
    project_share: bool,
) -> anyhow::Result<Vec<mount::ResolvedMount>> {
    let project_mount = project_share.then(|| mount::ResolvedMount {
        mount_type: mount::MountType::Dir {
            key: "project".to_string(),
        },
        source: project.host_cwd.clone(),
        target: project.guest_cwd.to_string_lossy().into(),
        read_only: false,
    });

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
        container_home,
        &project.host_cwd,
        &project.guest_cwd,
    )?;

    let mut mounts: Vec<_> = project_mount.into_iter().collect();
    mounts.extend(user_mounts);
    Ok(mounts)
}

/// Make the VirtioFS share list from the static shares, the dir mounts and
/// the file mounts.
///
/// File mounts are hardlinked (copied on EXDEV) into
/// `overlay/files/{rw,ro}/{key}`. Two shares give them to the guest.
fn prepare_shares(
    _image: &OciImage,
    mounts: &[mount::ResolvedMount],
    sandbox_dir: &Path,
) -> anyhow::Result<Vec<VmShare>> {
    // The guest makes the image rootfs with overlayfs from
    // `/mnt/layers/<d>`. Share the per-layer cache root one time. The guest
    // reads only the layers that `imageLayers` lists for this image.
    let mut shares = vec![VmShare {
        tag: "layers".to_string(),
        host_path: crate::cache::layers_root()?,
        read_only: true,
    }];

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

    // Hardlink file mounts into overlay/files/{rw|ro}/{key}. Make the dirs
    // again on each boot, so old entries are removed.
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

/// Build the kernel command line string.
fn build_kernel_cmdline(log_level: LogLevel) -> String {
    let mut cmdline = "console=hvc0 console=ttyS0 rdinit=/init".to_string();
    if !matches!(log_level, LogLevel::Trace | LogLevel::Debug) {
        cmdline.push_str(" quiet loglevel=3");
    }
    cmdline
}

/// Print the VM resources summary (cpus, memory, disk).
fn log_config(project: &Project) {
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
}

/// Start the platform-specific VM backend. The caller must wait until a
/// vsock port is ready, see [`VmInstance::vsock_connect`].
#[cfg_attr(not(target_os = "macos"), allow(clippy::unused_async))]
async fn boot_backend(vm_config: &config::VmConfig) -> anyhow::Result<Box<dyn VmHandle>> {
    #[cfg(target_os = "macos")]
    {
        let mut backend = apple::AppleVmBackend::new(vm_config)?;
        backend.start().await?;
        Ok(Box::new(backend))
    }

    #[cfg(target_os = "linux")]
    {
        let backend = cloud_hypervisor::CloudHypervisorBackend::start(vm_config)?;
        Ok(Box::new(backend))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = vm_config;
        Err(anyhow::anyhow!("unsupported platform"))
    }
}

/// Future returned by the object-safe [`VmHandle`] methods.
type HandleFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// Trait for VM backends. Dropping the handle kills the VM.
trait VmHandle {
    /// Open a new vsock connection to the given guest port. Used for the
    /// supervisor channel and for later channels, for example the network
    /// proxy.
    fn vsock_connect(&self, port: u32) -> HandleFuture<'_, anyhow::Result<OwnedFd>>;

    /// Stop the VM and wait until the backend confirms the stop. A drop of
    /// the handle after this does not stop the VM again.
    fn stop(&mut self) -> HandleFuture<'_, anyhow::Result<()>>;
}

#[cfg(target_os = "macos")]
impl VmHandle for apple::AppleVmBackend {
    fn vsock_connect(&self, port: u32) -> HandleFuture<'_, anyhow::Result<OwnedFd>> {
        Box::pin(apple::AppleVmBackend::vsock_connect(self, port))
    }

    fn stop(&mut self) -> HandleFuture<'_, anyhow::Result<()>> {
        Box::pin(apple::AppleVmBackend::stop(self))
    }
}

#[cfg(target_os = "linux")]
impl VmHandle for cloud_hypervisor::CloudHypervisorBackend {
    fn vsock_connect(&self, port: u32) -> HandleFuture<'_, anyhow::Result<OwnedFd>> {
        Box::pin(async move { cloud_hypervisor::CloudHypervisorBackend::vsock_connect(self, port) })
    }

    fn stop(&mut self) -> HandleFuture<'_, anyhow::Result<()>> {
        Box::pin(async move { cloud_hypervisor::CloudHypervisorBackend::stop(self) })
    }
}

#[cfg(test)]
mod tests;
