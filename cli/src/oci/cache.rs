use std::fs;
use std::path::{Path, PathBuf};

use crate::cli;
use crate::config::config::Cache;

/// Default disk size (10 GB) — used for overlay upper + cache dirs.
const DEFAULT_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Ensure the project disk image exists (for overlay upper + cache).
/// Always creates one — the disk backs both the rootfs overlay upper
/// layer and any configured cache mounts.
///
/// Returns (disk_image_path, cache_target_paths).
pub fn prepare(
    cache_dir: &Path,
    config: Option<&Cache>,
    container_home: &str,
    cwd: &Path,
) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let image_path = cache_dir.join("disk.img");

    let configured_bytes = config.map(|c| (c.size.0 + 511) & !511).filter(|&b| b > 0);
    let bytes = configured_bytes.unwrap_or((DEFAULT_DISK_BYTES + 511) & !511);

    if image_path.exists() {
        let current_size = fs::metadata(&image_path)?.len();
        if current_size > bytes {
            fs::remove_file(&image_path)?;
            create_sparse(&image_path, bytes)?;
            cli::log!(
                "  {} disk recreated {}",
                cli::check(),
                cli::dim(&format_size(bytes))
            );
        } else if current_size < bytes {
            grow_sparse(&image_path, bytes)?;
            cli::log!(
                "  {} disk grown to {}",
                cli::check(),
                cli::dim(&format_size(bytes))
            );
        }
    } else {
        create_sparse(&image_path, bytes)?;
        cli::log!(
            "  {} disk created {}",
            cli::check(),
            cli::dim(&format_size(bytes))
        );
    }

    let container_home = PathBuf::from(container_home);
    let empty = vec![];
    let cache_targets: Vec<String> = config
        .map_or(&empty, |c| &c.mounts)
        .iter()
        .map(|path| {
            let target = super::expand_tilde(path, &container_home);
            let target = if target.is_relative() {
                cwd.join(target)
            } else {
                target
            };
            target.to_string_lossy().into_owned()
        })
        .collect();

    Ok((image_path, cache_targets))
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{} GB", bytes / (1024 * 1024 * 1024))
    } else {
        format!("{} MB", bytes / (1024 * 1024))
    }
}

fn create_sparse(path: &Path, size: u64) -> anyhow::Result<()> {
    let file = fs::File::create(path)?;
    file.set_len(size)?;
    Ok(())
}

fn grow_sparse(path: &Path, size: u64) -> anyhow::Result<()> {
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(size)?;
    Ok(())
}
