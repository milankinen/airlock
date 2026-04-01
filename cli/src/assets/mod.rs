use crate::error::CliError;
use crate::oci::cache::cache_dir;
use std::path::PathBuf;

const KERNEL: &[u8] = include_bytes!("../../../sandbox/out/Image");
const INITRAMFS: &[u8] = include_bytes!("../../../sandbox/out/initramfs.gz");

pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

impl Assets {
    pub fn init() -> Result<Self, CliError> {
        let dir = cache_dir()?.join("kernel");
        let kernel = dir.join("Image");
        let initramfs = dir.join("initramfs.gz");

        if !kernel.exists() || !initramfs.exists() {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&kernel, KERNEL)?;
            std::fs::write(&initramfs, INITRAMFS)?;
        }

        Ok(Assets { kernel, initramfs })
    }
}
