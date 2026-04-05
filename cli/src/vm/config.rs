use std::path::PathBuf;

pub struct VmShare {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

#[allow(dead_code)]
pub struct VmConfig {
    pub cpus: u32,
    pub memory_bytes: u64,
    #[cfg(target_os = "macos")]
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    #[cfg(target_os = "macos")]
    pub kernel_cmdline: String,
    pub shares: Vec<VmShare>,
    /// Sparse raw disk image for cache volume (VirtIO block device).
    pub cache_disk: Option<PathBuf>,
    /// Directory for runtime files (e.g., vsock UNIX sockets).
    pub runtime_dir: PathBuf,
    /// Host ports to forward.
    pub host_ports: Vec<u16>,
}
