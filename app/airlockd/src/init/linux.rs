//! Linux guest VM initialization.
//!
//! Runs the initialization stages in the correct order. Also contains the
//! Linux implementation of clock sync and periodic resource maintenance.

use super::{InitConfig, MountConfig};
use crate::rpc::SocketForwardConfig;

mod ca;
mod clock;
mod container;
mod disk;
mod mount;
mod net;
mod overlay;

/// Apply the host wall-clock to `CLOCK_REALTIME`. See
/// [`super::set_clock`].
pub(super) fn set_clock(epoch: u64, epoch_nanos: u32) {
    clock::set(epoch, epoch_nanos);
}

/// Start the periodic disk trim and slab cache drop task. See
/// [`super::start_periodic_maintenance`].
pub(super) fn start_periodic_maintenance() {
    disk::start_periodic_maintenance();
}

/// Run all guest initialization steps in order, including container mounts.
/// See [`super::setup`] for the arguments.
///
/// The order is important:
///  * VirtioFS shares must be mounted before the overlay is assembled.
///  * Networking must be up before the proxy starts.
///  * The project disk must be ready before the overlayfs rootfs is
///    assembled.
///  * Container mounts (proc/sys/dev, file bind mounts) run after the
///    rootfs is assembled, so they have priority over the directory bind
///    mounts before them.
///  * The sandbox mount namespace is made last, so it has all the mounts
///    above.
pub fn setup(
    config: &InitConfig,
    mounts: &MountConfig,
    _sockets: &[SocketForwardConfig],
    nested_virt: bool,
) -> anyhow::Result<()> {
    clock::set(config.epoch, config.epoch_nanos);

    // 1. Mount well-known VirtioFS shares
    mount::virtiofs("layers")?;

    // 2. Mount user dir shares (includes "project" and "dir_N" mounts)
    for dir in &mounts.dirs {
        mount::virtiofs(&dir.tag)?;
    }

    // 3. Mount file-mount VirtioFS shares (these exist only if the config
    //    has file mounts)
    if mounts.files.iter().any(|f| !f.read_only) {
        mount::virtiofs("files/rw")?;
    }
    if mounts.files.iter().any(|f| f.read_only) {
        mount::virtiofs("files/ro")?;
    }

    // Create a local directory for the overlayfs mount point (it is not a
    // VirtioFS share).
    std::fs::create_dir_all("/mnt/overlay/rootfs")?;

    // 4. Networking
    net::setup(&config.host_ports)?;

    // 5. Project disk (ext4, with the overlayfs upper layer and the caches)
    disk::setup(&mounts.caches)?;

    // 6. Assemble container rootfs (overlayfs layers + dir/cache bind mounts)
    overlay::assemble(mounts)?;

    // 7. DNS
    net::setup_dns()?;

    // 8. Container mounts: proc/sys/dev, file bind mounts.
    //    Runs after overlay::assemble, so file bind mounts can replace paths
    //    inside directory bind mounts (for example guest_cwd).
    container::setup(mounts, nested_virt)?;

    // 9. The sandbox mount namespace, made from the finished rootfs. If it
    //    fails, spawns use chroot. That works, but tools that enter with
    //    setns (for example `docker exec`) then get the VM root.
    if let Err(e) = crate::sandbox_ns::create() {
        tracing::warn!("{e:#}; falling back to chroot");
    }

    Ok(())
}
