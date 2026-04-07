use std::path::PathBuf;

pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    #[cfg(target_os = "linux")]
    pub cloud_hypervisor: PathBuf,
    #[cfg(target_os = "linux")]
    pub virtiofsd: PathBuf,
}

impl Assets {
    #[cfg(not(test))]
    pub fn init() -> anyhow::Result<Assets> {
        const CHECKSUM: &str = env!("EZPEZ_ASSETS_CHECKSUM");

        let dir = crate::cache::cache_dir()?.join("kernel");
        let checksum_file = dir.join("checksum");

        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            std::fs::create_dir_all(&dir)?;

            std::fs::write(dir.join("Image"), include_bytes!("../../sandbox/out/Image"))?;
            std::fs::write(
                dir.join("initramfs.gz"),
                include_bytes!("../../sandbox/out/initramfs.gz"),
            )?;

            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::PermissionsExt;

                let ch = dir.join("cloud-hypervisor");
                std::fs::write(&ch, include_bytes!("../../sandbox/out/cloud-hypervisor"))?;
                std::fs::set_permissions(&ch, std::fs::Permissions::from_mode(0o755))?;

                let vfs = dir.join("virtiofsd");
                std::fs::write(&vfs, include_bytes!("../../sandbox/out/virtiofsd"))?;
                std::fs::set_permissions(&vfs, std::fs::Permissions::from_mode(0o755))?;
            }

            std::fs::write(&checksum_file, CHECKSUM)?;
        }

        Ok(Assets {
            kernel: dir.join("Image"),
            initramfs: dir.join("initramfs.gz"),
            #[cfg(target_os = "linux")]
            cloud_hypervisor: dir.join("cloud-hypervisor"),
            #[cfg(target_os = "linux")]
            virtiofsd: dir.join("virtiofsd"),
        })
    }

    #[cfg(test)]
    pub fn init() -> anyhow::Result<Assets> {
        anyhow::bail!("Assets::init not supported in tests")
    }
}
