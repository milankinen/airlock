use std::path::PathBuf;

pub struct VmConfig {
    pub cpus: u32,
    pub memory_bytes: u64,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub kernel_cmdline: String,
}
