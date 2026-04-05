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
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub kernel_cmdline: String,
    pub shares: Vec<VmShare>,
    /// Sparse raw disk image for cache volume (VirtIO block device).
    pub cache_disk: Option<PathBuf>,
    /// Directory for runtime files (e.g., vsock UNIX sockets).
    pub runtime_dir: PathBuf,
    /// Path to extracted initramfs root directory (Linux/libkrun only).
    #[cfg(target_os = "linux")]
    pub initramfs_root: PathBuf,
    /// Host ports to forward.
    pub host_ports: Vec<u16>,
}
