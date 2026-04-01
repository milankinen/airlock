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
    pub fn init() -> Result<Assets, CliError> {
        let dir = cache_dir()?.join("kernel");
        let kernel = dir.join("Image");
        let initramfs = dir.join("initramfs.gz");

        // Re-extract if sizes don't match (detects binary updates)
        let needs_update = !kernel.exists()
            || !initramfs.exists()
            || std::fs::metadata(&kernel).map(|m| m.len()).unwrap_or(0) != KERNEL.len() as u64
            || std::fs::metadata(&initramfs).map(|m| m.len()).unwrap_or(0)
                != INITRAMFS.len() as u64;

        if needs_update {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&kernel, KERNEL)?;
            std::fs::write(&initramfs, INITRAMFS)?;
        }

        Ok(Assets { kernel, initramfs })
    }
}
