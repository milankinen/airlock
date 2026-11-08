//! VM lifecycle support.
//!
//! Configures, boots and controls the sandbox VM, and connects to the
//! supervisor in the VM. Also resolves the configured mounts, extracts the
//! VM boot assets and manages the persistent sandbox disk. On Linux, it
//! checks that the user can use KVM.
//!
//! The `vmm` setting selects the VM backend: the Apple Virtualization
//! framework (macOS), Cloud Hypervisor with virtiofsd (Linux) or libkrun.

mod apple_virt;
mod assets;
mod backend;
mod cloud_hypervisor;
mod config;
pub(crate) mod disk;
mod file_sync;
#[cfg(target_os = "linux")]
mod kvm_status;
pub mod libkrun;
pub mod mount;

use std::os::unix::io::OwnedFd;

use self::backend::VmHandle;
pub use self::backend::{IdMap, VmmBackend, load_backend};
#[cfg(target_os = "linux")]
pub use self::kvm_status::{KvmStatus, kvm_status, require_kvm};
use crate::cli::LogLevel;
use crate::data_dir::DataDir;
use crate::oci::OciImage;
use crate::sandbox::{BootOptions, Project};
use crate::util::VerifiedPath;
use crate::vm::config::VmShare;
use crate::{cli, messages};

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
    pub disk_image: VerifiedPath,
    /// Cache directories on the sandbox disk.
    pub caches: Vec<disk::CacheEntry>,
    /// Resolved guest home directory.
    pub container_home: String,
    /// The sandbox env (see [`crate::sandbox::process::vm::boot::guest_env`]).
    pub env: Vec<String>,
    /// Working directory in the guest.
    pub cwd: String,
    /// Container uid.
    pub uid: u32,
    /// Container gid.
    pub gid: u32,
    /// Id maps of the image layers and the other shares. `None`: no maps.
    pub id_map: Option<(IdMap, IdMap)>,
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
                // A VM that stopped never answers.
                Err(e) if e.is::<backend::VmStopped>() => return Err(e),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(DELAY).await;
                }
            }
        }
        Err(anyhow::anyhow!(messages::vm::vsock_connect_retries_failed(
            port,
            MAX_ATTEMPTS,
            last_err.expect("at least one attempt")
        )))
    }
}

/// Boot the sandbox VM and connect to the in-VM supervisor.
/// Args:
///  - `project`: The project with its config
///  - `image`: The prepared OCI image
///  - `container_home`: Resolved guest home, see
///    [`crate::oci::effective_container_home`]. Used for `~/...` expansion
///    of mount, cache and socket-forward paths
///  - `env`: The sandbox env ([`crate::sandbox::process::vm::boot::guest_env`])
///  - `opts`: Boot options. `vmm` boots the VM. If `project_share` is false,
///    there is no project mount and the guest runs in `/`. `quiet` skips the
///    VM resources summary.
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
    let backend = opts.vmm;
    tracing::debug!("vm backend: {}", backend.kind());
    let boot_files = assets::boot_files(project)?;
    let sandbox_dir = project.sandbox_dir.clone();
    let overlay_dir = sandbox_dir.join("overlay")?;

    let mounts = assemble_mounts(project, container_home, opts.project_share)?;
    let shares = prepare_shares(&project.context.data, &mounts, &overlay_dir)?;
    let (disk_image, caches) = disk::prepare(
        &sandbox_dir,
        &project.config.disk,
        container_home,
        &project.host_cwd.to_string(),
    )?;
    let cwd = if opts.project_share {
        project.guest_cwd.clone()
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
            share.host_path,
            share.read_only
        );
    }

    let vm_config = config::VmConfig {
        cpus: project.config.vm.cpus,
        memory_bytes: project.config.vm.memory.0,
        kernel: boot_files.kernel,
        initramfs: boot_files.initramfs,
        kernel_cmdline: build_kernel_cmdline(backend.kernel_console(), opts.log_level),
        shares,
        cache_disk: Some(disk_image.clone()),
        runtime_dir: sandbox_dir,
        asset_dir: boot_files.dir,
        kvm: project.config.vm.kvm,
    };

    let vm_handle = backend.boot_vm(&vm_config).await?;
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
        id_map: backend.id_map(image.uid, image.gid),
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
    let project_mount = if project_share {
        Some(mount::ResolvedMount {
            mount_type: mount::MountType::Dir {
                key: "project".to_string(),
            },
            name: "project".to_string(),
            given: project.host_cwd.to_string(),
            source: project.host_cwd.clone(),
            target: project.guest_cwd.clone(),
            read_only: false,
        })
    } else {
        None
    };

    let enabled_mounts = mount::enabled_mounts(&project.config);
    let user_mounts = mount::resolve_mounts(
        &enabled_mounts,
        &project.host_home,
        container_home,
        &project.host_cwd,
        &project.guest_cwd,
        project_share,
    )?;

    let mut mounts: Vec<_> = project_mount.into_iter().collect();
    mounts.extend(user_mounts);
    for m in &mounts {
        let resolved = m.source.to_path().ok();
        tracing::debug!("mount source: {} = {:?} → {resolved:?}", m.name, m.given);
    }
    if !project.context.settings.security.insecure_mounts {
        let context = &project.context;
        mount::check_exposure(
            &mounts,
            &[
                (messages::vm::HOME_DIRECTORY, &context.home),
                (messages::vm::DATA_DIRECTORY, &context.data.root),
            ],
        )?;
    }
    Ok(mounts)
}

/// Make the VirtioFS share list from the static shares, the dir mounts and
/// the file mounts.
///
/// File mounts are hardlinked (copied on EXDEV) into
/// `overlay/files/{rw,ro}/{key}`. Two shares give them to the guest.
fn prepare_shares(
    data_dir: &DataDir,
    mounts: &[mount::ResolvedMount],
    overlay_dir: &VerifiedPath,
) -> anyhow::Result<Vec<VmShare>> {
    // The guest makes the image rootfs with overlayfs from
    // `/mnt/layers/<d>`. Share the per-layer cache root one time. The guest
    // reads only the layers that `imageLayers` lists for this image.
    let mut shares = vec![VmShare {
        tag: "layers".to_string(),
        host_path: crate::cache::layers_root(data_dir)?,
        read_only: true,
        // A layer dir never changes after the download.
        immutable: true,
    }];

    for m in mounts
        .iter()
        .filter(|m| matches!(m.mount_type, mount::MountType::Dir { .. }))
    {
        tracing::debug!(
            "mount: {} → {} → {} (read-only: {})",
            m.source,
            m.vm_path(),
            m.target,
            m.read_only
        );
        shares.push(VmShare {
            tag: m.key().into(),
            host_path: m.source.clone(),
            read_only: m.read_only,
            immutable: false,
        });
    }

    // Hardlink file mounts into overlay/files/{rw|ro}/{key}. Make the dirs
    // again on each boot, so old entries are removed.
    let _ = overlay_dir.join("files/rw")?.remove_dir_all();
    let _ = overlay_dir.join("files/ro")?.remove_dir_all();
    // Join after the removal. A path keeps the identity of the old dir, and
    // the check then refuses the new dir.
    let files_rw_dir = overlay_dir.join("files/rw")?;
    let files_ro_dir = overlay_dir.join("files/ro")?;
    let mut has_rw_files = false;
    let mut has_ro_files = false;

    for m in mounts
        .iter()
        .filter(|m| matches!(m.mount_type, mount::MountType::File { .. }))
    {
        tracing::debug!(
            "file mount: {} → {} (read-only: {})",
            m.source,
            m.target,
            m.read_only
        );
        let dir = if m.read_only {
            &files_ro_dir
        } else {
            &files_rw_dir
        };
        dir.mkdirs()?;
        let link_path = dir.join(m.key())?;
        if let Err(e) = m.source.hard_link_to(&link_path) {
            if e.kind() == std::io::ErrorKind::CrossesDevices {
                let text = messages::vm::file_mount_cross_device(&m.source);
                cli::warn!(text, "{text}");
                m.source.copy_to(&link_path)?;
            } else {
                return Err(
                    anyhow::Error::from(e).context(messages::vm::file_mount_failed(&m.source))
                );
            }
        }
        has_ro_files = has_ro_files || m.read_only;
        has_rw_files = has_rw_files || !m.read_only;
    }

    // The share dirs are new: check them again to pin them.
    if has_rw_files {
        shares.push(VmShare {
            tag: "files/rw".to_string(),
            host_path: files_rw_dir.reverify()?,
            read_only: false,
            immutable: false,
        });
    }
    if has_ro_files {
        shares.push(VmShare {
            tag: "files/ro".to_string(),
            host_path: files_ro_dir.reverify()?,
            read_only: true,
            immutable: false,
        });
    }

    Ok(shares)
}

/// Build the kernel command line string.
/// Args:
///  - `console`: Console part of the command line from the VM backend
///  - `log_level`: Log level of the boot
fn build_kernel_cmdline(console: &str, log_level: LogLevel) -> String {
    // A backend without a console gives an empty console part.
    let mut cmdline = format!("{console} rdinit=/init").trim_start().to_string();
    if !matches!(log_level, LogLevel::Trace | LogLevel::Debug) {
        cmdline.push_str(" quiet loglevel=3");
    }
    cmdline
}

/// Print the VM resources summary (cpus, memory, disk).
fn log_config(project: &Project) {
    cli::log!(
        "  {} {}",
        cli::bullet(),
        messages::vm::config_cpus(cli::dim(&project.config.vm.cpus.to_string()))
    );
    cli::log!(
        "  {} {}",
        cli::bullet(),
        messages::vm::config_memory(cli::dim(&project.config.vm.memory.to_string()))
    );
    cli::log!(
        "  {} {}",
        cli::bullet(),
        messages::vm::config_disk(cli::dim(&project.config.disk.size.to_string()))
    );
}

/// Remove the old entry at `path`, if any. Never follows a symlink.
/// A directory is removed too, so a planted directory cannot block a start.
#[cfg(any(target_os = "linux", vm_libkrun))]
fn remove_old(path: &VerifiedPath) -> std::io::Result<()> {
    let is_dir = path.symlink_metadata().is_ok_and(|m| m.is_dir());
    let result = if is_dir {
        path.remove_dir_all()
    } else {
        path.remove_file()
    };
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
