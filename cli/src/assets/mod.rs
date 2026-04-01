use crate::error::CliError;
use std::path::PathBuf;

const KERNEL: &[u8] = include_bytes!("../../../sandbox/out/Image");
const INITRAMFS: &[u8] = include_bytes!("../../../sandbox/out/initramfs.gz");

pub struct AssetPaths {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    // Keep the temp dir alive so files aren't deleted
    pub _tmp: Option<tempfile::TempDir>,
}

/// Write embedded kernel and initramfs to temp files.
/// VZLinuxBootLoader requires file URLs, so we can't pass bytes directly.
pub fn extract_assets() -> Result<AssetPaths, CliError> {
    let tmp = tempfile::tempdir()?;
    let kernel = tmp.path().join("Image");
    let initramfs = tmp.path().join("initramfs.gz");
    std::fs::write(&kernel, KERNEL)?;
    std::fs::write(&initramfs, INITRAMFS)?;
    Ok(AssetPaths {
        kernel,
        initramfs,
        _tmp: Some(tmp),
    })
}
