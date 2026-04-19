//! Linux-specific guest VM initialization sequencer.
//!
//! Each stage lives in its own submodule; this file is just the
//! ordering glue. The ordering matters: VirtioFS shares must be
//! mounted before we can assemble the overlay, networking must be
//! up before the proxy starts, and the project disk must be ready
//! before the overlayfs rootfs is assembled. Container mounts
//! (proc/sys/dev, file bind mounts) run last so they take precedence
//! over earlier dir bind mounts.

use super::{InitConfig, MountConfig};
use crate::rpc::SocketForwardConfig;

mod ca;
mod clock;
mod container;
mod disk;
mod mount;
mod net;
mod overlay;

/// Run all guest initialization steps in order, including container mounts.
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

    // 3. Mount file-mount VirtioFS shares (present only if config has file mounts)
    if mounts.files.iter().any(|f| !f.read_only) {
        mount::virtiofs("files/rw")?;
    }
    if mounts.files.iter().any(|f| f.read_only) {
        mount::virtiofs("files/ro")?;
    }

    // Create local directory for the overlayfs mount point (no longer a VirtioFS share).
    std::fs::create_dir_all("/mnt/overlay/rootfs")?;

    // 4. Networking
    net::setup(&config.host_ports)?;

    // 5. Project disk (ext4 — overlayfs upper + cache)
    disk::setup(&mounts.caches)?;

    // 6. Assemble container rootfs (overlayfs layers + dir/cache bind mounts)
    overlay::assemble(mounts)?;

    // 7. DNS
    net::setup_dns()?;

    // 8. Container mounts: proc/sys/dev, file bind mounts.
    //    Runs after overlay::assemble so file bind mounts can override paths
    //    inside dir-bind-mounted directories (e.g. guest_cwd).
    container::setup(mounts, nested_virt)?;

    Ok(())
}
