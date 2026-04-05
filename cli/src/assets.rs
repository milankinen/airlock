use std::path::PathBuf;

pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    #[cfg(target_os = "linux")]
    pub libkrun: PathBuf,
    #[cfg(target_os = "linux")]
    pub libkrunfw: PathBuf,
    #[cfg(target_os = "linux")]
    pub initramfs_root: PathBuf,
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

            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::PermissionsExt;

                let libkrun = dir.join("libkrun.so");
                std::fs::write(&libkrun, include_bytes!("../../sandbox/out/libkrun.so"))?;
                std::fs::set_permissions(&libkrun, std::fs::Permissions::from_mode(0o755))?;

                let libkrunfw = dir.join("libkrunfw.so");
                std::fs::write(&libkrunfw, include_bytes!("../../sandbox/out/libkrunfw.so"))?;
                std::fs::set_permissions(&libkrunfw, std::fs::Permissions::from_mode(0o755))?;

                // Extract initramfs to a directory for krun_set_root
                let rootfs_dir = dir.join("rootfs");
                if rootfs_dir.exists() {
                    std::fs::remove_dir_all(&rootfs_dir)?;
                }
                std::fs::create_dir_all(&rootfs_dir)?;
                extract_initramfs(&initramfs, &rootfs_dir)?;
            }

            std::fs::write(&checksum_file, CHECKSUM)?;
        }

        Ok(Assets {
            kernel,
            initramfs,
            #[cfg(target_os = "linux")]
            libkrun: dir.join("libkrun.so"),
            #[cfg(target_os = "linux")]
            libkrunfw: dir.join("libkrunfw.so"),
            #[cfg(target_os = "linux")]
            initramfs_root: dir.join("rootfs"),
        })
    }

    #[cfg(test)]
    pub fn init() -> anyhow::Result<Assets> {
        anyhow::bail!("Assets::init not supported in tests")
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn extract_initramfs(
    initramfs_gz: &std::path::Path,
    target_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use std::process::Command;
    // Use system cpio to extract the gzipped cpio archive
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd {} && gzip -dc {} | cpio -id --quiet 2>/dev/null",
            target_dir.display(),
            initramfs_gz.display()
        ))
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to extract initramfs");
    }
    Ok(())
}
