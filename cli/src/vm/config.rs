use std::path::PathBuf;

pub struct VmShare {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

pub struct VmConfig {
    pub cpus: u32,
    pub memory_bytes: u64,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub kernel_cmdline: String,
    pub shares: Vec<VmShare>,
}
