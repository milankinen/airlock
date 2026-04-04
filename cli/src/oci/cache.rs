use std::fs;
use std::path::{Path, PathBuf};

use crate::cli;
use crate::config::config::Cache;
use crate::oci::{MountType, ResolvedMount};

/// Prepare the cache disk image and return the image path + cache mounts.
///
/// Returns `(None, [])` if cache is not configured.
pub fn prepare(
    cache_dir: &Path,
    config: Option<&Cache>,
    container_home: &str,
) -> anyhow::Result<(Option<PathBuf>, Vec<ResolvedMount>)> {
    let image_path = cache_dir.join("cache.img");
    let Some(config) = config.filter(|c| !c.mounts.is_empty()) else {
        if image_path.exists() {
            fs::remove_file(&image_path)?;
            cli::log!("  {} cache volume removed", cli::check());
        }
        return Ok((None, vec![]));
    };

    // Align to 512-byte blocks (required by VZDiskImageStorageDeviceAttachment)
    let size = config.size;
    let bytes = (size.0 + 511) & !511;

    if image_path.exists() {
        let current_size = fs::metadata(&image_path)?.len();
        if current_size > bytes {
            // Shrink: delete and recreate (ext4 will be reformatted)
            fs::remove_file(&image_path)?;
            create_sparse(&image_path, bytes)?;
            cli::log!(
                "  {} cache volume recreated {}",
                cli::check(),
                cli::dim(&size.to_string())
            );
        } else if current_size < bytes {
            // Grow: extend the sparse file (resize2fs in init will expand fs)
            grow_sparse(&image_path, bytes)?;
            cli::log!(
                "  {} cache volume grown to {}",
                cli::check(),
                cli::dim(&size.to_string())
            );
        }
    } else {
        create_sparse(&image_path, bytes)?;
        cli::log!(
            "  {} cache volume created {}",
            cli::check(),
            cli::dim(&size.to_string())
        );
    }

    let container_home = PathBuf::from(container_home);
    let mounts: Vec<ResolvedMount> = config
        .mounts
        .iter()
        .map(|path| {
            let target = super::expand_tilde(path, &container_home);
            let target_str = target.to_string_lossy().into_owned();
            // Use the absolute target path (without leading /) as subdir
            let subdir = target_str
                .strip_prefix('/')
                .unwrap_or(&target_str)
                .to_string();
            ResolvedMount {
                display: Some((path.clone(), path.clone())),
                mount_type: MountType::Cache { subdir },
                source: image_path.clone(),
                target: target_str,
                read_only: false,
            }
        })
        .collect();

    Ok((Some(image_path), mounts))
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
