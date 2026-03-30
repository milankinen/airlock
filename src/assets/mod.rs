pub mod alpine;

use crate::error::Result;
use std::path::PathBuf;

pub struct AssetPaths {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

pub async fn ensure_assets() -> Result<AssetPaths> {
    alpine::ensure_alpine_assets().await
}
