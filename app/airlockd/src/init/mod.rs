//! Guest VM initialization.
//!
//! Prepares the guest at boot: sets the clock, mounts the filesystems, sets up
//! networking and assembles the container rootfs. Also syncs the guest clock
//! with the host on request, and frees unused resources at intervals.
//!
//! Only Linux has a real implementation. On other targets, stubs let the crate
//! compile.

/// Parameters from the host CLI for guest VM initialization.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct InitConfig {
    /// Wall-clock time for the guest system clock (seconds since the Unix
    /// epoch). VMs have no RTC, so the host gives the current time.
    pub epoch: u64,
    /// Nanoseconds part of the wall-clock time.
    pub epoch_nanos: u32,
    /// Host TCP ports whose traffic goes through the network proxy, so the
    /// sandbox can intercept localhost traffic.
    pub host_ports: Vec<u16>,
}

/// A directory mount: a VirtioFS tag mapped to a container path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct DirMountConfig {
    /// VirtioFS tag of the share.
    pub tag: String,
    /// Mount path inside the container.
    pub target: String,
    /// Mount the share read-only.
    pub read_only: bool,
}

/// A file mount.
///
/// On the host, the file is hard-linked (or copied if a hard link fails)
/// into the project's `overlay/files/{rw|ro}/{mount_key}` directory. The
/// `files/rw` or `files/ro` VirtioFS share makes it available to the guest.
/// Inside the container, `target` becomes a symlink to
/// `/airlock/.files/{rw|ro}/{mount_key}`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct FileMountConfig {
    /// Config key of the mount. Also the file name in the VirtioFS share
    /// directory.
    pub mount_key: String,
    /// File path inside the container.
    pub target: String,
    /// Mount the file read-only.
    pub read_only: bool,
}

/// A named persistent cache mount on the project disk.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CacheConfig {
    /// Cache name. Also the directory name under `/mnt/disk/cache`.
    pub name: String,
    /// The cache is mounted only if `true`.
    pub enabled: bool,
    /// Container paths that use the cache.
    pub paths: Vec<String>,
}

/// A `[mask.<name>]` block: project-relative paths that get an empty
/// directory bind-mounted over them, so the sandbox cannot see them.
///
/// Recreated on every VM start. Each mask has its own source directories
/// under `/mnt/disk/mask/project/<name>`, so the blocks stay isolated.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MaskConfig {
    /// Mask name from `[mask.<name>]`.
    pub name: String,
    /// Project-relative paths to hide.
    pub paths: Vec<String>,
}

/// All mount configuration from the host, received in `Supervisor.boot()`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MountConfig {
    /// ID of the container image.
    pub image_id: String,
    /// Layer digests of the image rootfs, topmost first. Each digest names
    /// a directory `/mnt/layers/<digest>/`, used as an overlayfs lowerdir.
    pub image_layers: Vec<String>,
    /// Directory mounts.
    pub dirs: Vec<DirMountConfig>,
    /// File mounts.
    pub files: Vec<FileMountConfig>,
    /// Persistent cache mounts.
    pub caches: Vec<CacheConfig>,
    /// Hidden project paths.
    pub masks: Vec<MaskConfig>,
    /// Project CA cert (PEM bytes). Empty when the project has no CA.
    ///
    /// Guest init adds a non-empty cert to the CA bundles of the image after
    /// it mounts the overlayfs rootfs. Thus TLS clients in the container
    /// trust the MITM proxy of the sandbox, with no host-side overlay layer.
    pub ca_cert: Vec<u8>,
}

#[cfg(target_os = "linux")]
mod linux;

/// Prepare the guest VM environment: clock, mounts, networking, rootfs and
/// all mounts inside the container (proc/sys/dev, file bind mounts).
/// Args:
///  - `config`: Clock and network parameters from the host
///  - `mounts`: Mount configuration from the host
///  - `sockets`: Socket forwards from the host (not used at the moment)
///  - `nested_virt`: Give the container access to nested virtualization
#[cfg(target_os = "linux")]
pub fn setup(
    config: &InitConfig,
    mounts: &MountConfig,
    sockets: &[crate::rpc::SocketForwardConfig],
    nested_virt: bool,
) -> anyhow::Result<()> {
    linux::setup(config, mounts, sockets, nested_virt)
}

/// Stub for non-Linux hosts.
#[cfg(not(target_os = "linux"))]
pub fn setup(
    _config: &InitConfig,
    _mounts: &MountConfig,
    _sockets: &[crate::rpc::SocketForwardConfig],
    _nested_virt: bool,
) -> anyhow::Result<()> {
    unimplemented!("supervisor only runs inside the Linux VM");
}

/// Set the guest clock to the host wall-clock time. Safe to call more than
/// once. [`setup`] calls it at boot. The `Supervisor.syncClock` RPC calls it
/// later at intervals, to correct the drift after the host sleeps.
/// Args:
///  - `epoch`: Seconds since the Unix epoch. 0 means "do not set".
///  - `epoch_nanos`: Nanoseconds part of the time
#[cfg(target_os = "linux")]
pub fn set_clock(epoch: u64, epoch_nanos: u32) {
    linux::set_clock(epoch, epoch_nanos);
}

/// Non-Linux stub. Does nothing.
#[cfg(not(target_os = "linux"))]
pub fn set_clock(_epoch: u64, _epoch_nanos: u32) {}

/// Start a background task that gives sandbox resources back to the host
/// every 10 minutes:
///  * Trims the free space of the sparse project disk image.
///  * Drops the kernel dentry/inode slab, so the host virtiofs proxy can
///    close its cached FDs.
#[cfg(target_os = "linux")]
pub fn start_periodic_maintenance() {
    linux::start_periodic_maintenance();
}

/// Non-Linux stub. Does nothing.
#[cfg(not(target_os = "linux"))]
pub fn start_periodic_maintenance() {}
