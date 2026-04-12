/// Parameters passed from the host CLI that influence guest VM initialization.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct InitConfig {
    /// Wall-clock epoch (seconds since Unix epoch) to set as the guest system
    /// clock. VMs don't have an RTC, so the host provides the current time.
    pub epoch: u64,
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

/// A file mount: symlinked into the container rootfs via /ez/.files/{rw,ro}.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct FileMountConfig {
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
    pub dirs: Vec<DirMountConfig>,
    pub files: Vec<FileMountConfig>,
    pub caches: Vec<CacheConfig>,
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
