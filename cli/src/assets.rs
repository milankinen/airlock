use std::path::PathBuf;

pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

impl Assets {
    #[cfg(not(test))]
    pub fn init() -> anyhow::Result<Assets> {
        const CHECKSUM: &str = env!("EZPEZ_ASSETS_CHECKSUM");

        let dir = crate::cache::cache_dir()?.join("kernel");
        let kernel = dir.join("Image");
        let initramfs = dir.join("initramfs.gz");
        let checksum_file = dir.join("checksum");

        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&kernel, include_bytes!("../../sandbox/out/Image"))?;
            std::fs::write(&initramfs, include_bytes!("../../sandbox/out/initramfs.gz"))?;
            std::fs::write(&checksum_file, CHECKSUM)?;
        }

        Ok(Assets { kernel, initramfs })
    }

    #[cfg(test)]
    pub fn init() -> anyhow::Result<Assets> {
        anyhow::bail!("Assets::init not supported in tests")
    }
}
