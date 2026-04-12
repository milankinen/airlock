//! Platform-independent VM configuration structs.

use std::path::PathBuf;

/// A VirtioFS shared directory between host and guest.
pub struct VmShare {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

/// Full VM configuration, consumed by the platform-specific backend.
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
    /// Path to cloud-hypervisor binary (Linux only).
    #[cfg(target_os = "linux")]
    pub cloud_hypervisor: PathBuf,
    /// Path to virtiofsd binary (Linux only).
    #[cfg(target_os = "linux")]
    pub virtiofsd: PathBuf,
    /// Enable KVM nested virtualization (Linux only).
    #[cfg(target_os = "linux")]
    pub kvm: bool,
}
