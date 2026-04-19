/// Parameters passed from the host CLI that influence guest VM initialization.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct InitConfig {
    /// Wall-clock time to set as the guest system clock (seconds since Unix
    /// epoch). VMs don't have an RTC, so the host provides the current time.
    pub epoch: u64,
    /// Sub-second nanoseconds component of the wall-clock time.
    pub epoch_nanos: u32,
    /// Host TCP ports whose traffic should be redirected through the network
    /// proxy (iptables REDIRECT) so the sandbox can intercept localhost traffic.
    pub host_ports: Vec<u16>,
}

/// A directory mount: a VirtioFS tag mapped to a container path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct DirMountConfig {
    pub tag: String,
    pub target: String,
    pub read_only: bool,
}

/// A file mount: the file is hard-linked (with copy fallback) into the project's
/// `overlay/files/{rw|ro}/{mount_key}` directory on the host and exposed via the
/// `files/rw` or `files/ro` VirtioFS share. Inside the container, `target`
/// becomes a symlink → `/airlock/.files/{rw|ro}/{mount_key}`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct FileMountConfig {
    /// Config key identifying the mount (used as filename in the VirtioFS share dir).
    pub mount_key: String,
    pub target: String,
    pub read_only: bool,
}

/// A named persistent cache mount backed by the project disk.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CacheConfig {
    pub name: String,
    pub enabled: bool,
    pub paths: Vec<String>,
}

/// All mount configuration received from the host via the start RPC.
/// Replaces the mounts.json file previously written to the overlay share.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct MountConfig {
    pub image_id: String,
    /// Ordered layer digests (topmost-first) composing the image rootfs. Each
    /// entry names a subdirectory under `/mnt/layers/<digest>/rootfs/` that
    /// will be used as an overlayfs lowerdir.
    pub image_layers: Vec<String>,
    pub dirs: Vec<DirMountConfig>,
    pub files: Vec<FileMountConfig>,
    pub caches: Vec<CacheConfig>,
    /// Project CA cert (PEM bytes). Empty when the project has no CA. When
    /// non-empty, guest init appends it to the image's CA bundles after the
    /// overlayfs rootfs is mounted, so TLS clients in the container trust
    /// the sandbox's MITM proxy without needing a host-side overlay layer.
    pub ca_cert: Vec<u8>,
}

#[cfg(target_os = "linux")]
mod linux;

/// Bootstrap the guest VM environment: clock, mounts, networking, rootfs,
/// and all container-internal mounts (proc/sys/dev, file bind mounts).
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
