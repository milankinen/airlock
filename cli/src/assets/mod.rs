use crate::oci::cache::cache_dir;
use std::path::PathBuf;

const KERNEL: &[u8] = include_bytes!("../../../sandbox/out/Image");
const INITRAMFS: &[u8] = include_bytes!("../../../sandbox/out/initramfs.gz");
const CHECKSUM: &str = env!("EZPEZ_ASSETS_CHECKSUM");

pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

impl Assets {
    pub fn init() -> anyhow::Result<Assets> {
        let dir = cache_dir()?.join("kernel");
        let kernel = dir.join("Image");
        let initramfs = dir.join("initramfs.gz");
        let checksum_file = dir.join("checksum");

        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&kernel, KERNEL)?;
            std::fs::write(&initramfs, INITRAMFS)?;
            std::fs::write(&checksum_file, CHECKSUM)?;
        }

        Ok(Assets { kernel, initramfs })
    }
}
