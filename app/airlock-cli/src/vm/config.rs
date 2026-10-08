//! Platform-independent VM configuration.
//!
//! Describes the VM to boot, including its resources and the host
//! directories that it shares. All platform VM backends use this description.

use std::path::PathBuf;

/// A VirtioFS directory that the host shares with the guest.
pub struct VmShare {
    /// VirtioFS tag that the guest uses to mount the share.
    pub tag: String,
    /// Shared directory on the host.
    pub host_path: PathBuf,
    /// If `true`, the guest cannot write to the share.
    pub read_only: bool,
}

/// Full VM configuration for the platform-specific backend.
#[allow(dead_code)]
pub struct VmConfig {
    /// Number of virtual CPUs.
    pub cpus: u32,
    /// Guest memory size in bytes.
    pub memory_bytes: u64,
    /// Path to the guest kernel.
    pub kernel: PathBuf,
    /// Path to the guest initramfs.
    pub initramfs: PathBuf,
    /// Kernel command line.
    pub kernel_cmdline: String,
    /// VirtioFS shares.
    pub shares: Vec<VmShare>,
    /// Sparse raw disk image for the cache volume (VirtIO block device).
    pub cache_disk: Option<PathBuf>,
    /// Directory for runtime files (e.g., vsock UNIX sockets).
    pub runtime_dir: PathBuf,
    /// Path to cloud-hypervisor binary (Linux only).
    #[cfg(target_os = "linux")]
    pub cloud_hypervisor: PathBuf,
    /// Path to virtiofsd binary (Linux only).
    #[cfg(target_os = "linux")]
    pub virtiofsd: PathBuf,
    /// Enable nested virtualization and give the guest access to KVM.
    pub kvm: bool,
}
