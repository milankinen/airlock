use std::path::PathBuf;

pub struct Assets {
    #[cfg(target_os = "macos")]
    pub kernel: PathBuf,
    /// On macOS: path to initramfs.gz. On Linux: path to extracted rootfs directory.
    pub initramfs: PathBuf,
    #[cfg(target_os = "linux")]
    pub libkrun: PathBuf,
    #[cfg(target_os = "linux")]
    pub libkrunfw: PathBuf,
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

            #[cfg(target_os = "macos")]
            {
                std::fs::write(
                    dir.join("initramfs.gz"),
                    include_bytes!("../../sandbox/out/initramfs.gz"),
                )?;
                std::fs::write(dir.join("Image"), include_bytes!("../../sandbox/out/Image"))?;
            }

            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::PermissionsExt;

                let libkrun = dir.join("libkrun.so");
                std::fs::write(&libkrun, include_bytes!("../../sandbox/out/libkrun.so"))?;
                std::fs::set_permissions(&libkrun, std::fs::Permissions::from_mode(0o755))?;

                let libkrunfw = dir.join("libkrunfw.so");
                std::fs::write(&libkrunfw, include_bytes!("../../sandbox/out/libkrunfw.so"))?;
                std::fs::set_permissions(&libkrunfw, std::fs::Permissions::from_mode(0o755))?;

                let rootfs_dir = dir.join("rootfs");
                if rootfs_dir.exists() {
                    std::fs::remove_dir_all(&rootfs_dir)?;
                }
                std::fs::create_dir_all(&rootfs_dir)?;
                extract_tar_gz(
                    include_bytes!("../../sandbox/out/rootfs.tar.gz"),
                    &rootfs_dir,
                )?;
            }

            std::fs::write(&checksum_file, CHECKSUM)?;
        }

        #[cfg(target_os = "macos")]
        let initramfs = dir.join("initramfs.gz");
        #[cfg(target_os = "linux")]
        let initramfs = dir.join("rootfs");

        Ok(Assets {
            #[cfg(target_os = "macos")]
            kernel: dir.join("Image"),
            initramfs,
            #[cfg(target_os = "linux")]
            libkrun: dir.join("libkrun.so"),
            #[cfg(target_os = "linux")]
            libkrunfw: dir.join("libkrunfw.so"),
        })
    }

    #[cfg(test)]
    pub fn init() -> anyhow::Result<Assets> {
        anyhow::bail!("Assets::init not supported in tests")
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn extract_tar_gz(data: &[u8], target_dir: &std::path::Path) -> anyhow::Result<()> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.unpack(target_dir)?;
    Ok(())
}
