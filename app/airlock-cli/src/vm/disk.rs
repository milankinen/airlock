//! Sparse disk image management for the project's persistent overlay and cache.

use std::fs;
use std::path::{Path, PathBuf};

use crate::cli;
use crate::config::config::Disk;

/// Default disk size (10 GB) — used for overlay upper + cache dirs.
const DEFAULT_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Ensure the project disk image exists (for overlay upper + cache).
/// Always creates one — the disk backs both the rootfs overlay upper
/// layer and any configured cache mounts.
///
/// Returns `(disk_image_path, cache_entries)` where each entry is
/// `(name, enabled, expanded_container_paths)`.
/// Named cache entry: `(name, enabled, expanded_container_paths)`.
pub type CacheEntry = (String, bool, Vec<String>);

pub fn prepare(
    cache_dir: &Path,
    config: &Disk,
    container_home: &str,
    cwd: &Path,
) -> anyhow::Result<(PathBuf, Vec<CacheEntry>)> {
    let image_path = cache_dir.join("disk.img");

    let bytes = (config.size.0 + 511) & !511;
    let bytes = if bytes > 0 {
        bytes
    } else {
        (DEFAULT_DISK_BYTES + 511) & !511
    };

    if image_path.exists() {
        let current_size = fs::metadata(&image_path)?.len();
        if current_size > bytes {
            // The disk backs the overlay upper layer (all in-sandbox writes)
            // and the named caches, so shrinking it destroys that data. Only
            // do it on explicit confirmation, then recreate the image from
            // scratch at the smaller size — so the user never has to delete
            // the file by hand. Declining (or no TTY) keeps the larger disk.
            if prompt_shrink_disk(current_size, bytes)? {
                fs::remove_file(&image_path)?;
                create_sparse(&image_path, bytes)?;
                cli::log!(
                    "  {} disk recreated {} (previous data erased)",
                    cli::check(),
                    cli::dim(&format_size(bytes))
                );
            } else {
                cli::log!(
                    "  {} disk kept at {} (configured {} is smaller; data preserved)",
                    cli::yellow("!"),
                    cli::dim(&format_size(current_size)),
                    cli::dim(&format_size(bytes))
                );
            }
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
    // Include all entries (enabled and disabled) so the supervisor
    // knows every declared name — it will clean up disk dirs for any
    // name not present in this list, and skip mounting disabled ones.
    let cache_entries: Vec<(String, bool, Vec<String>)> = config
        .cache
        .iter()
        .map(|(name, m)| {
            let paths = m
                .paths
                .iter()
                .map(|p| {
                    let target = crate::util::expand_tilde(p, &container_home);
                    let target = if target.is_relative() {
                        cwd.join(target)
                    } else {
                        target
                    };
                    target.to_string_lossy().into_owned()
                })
                .collect();
            (name.clone(), m.enabled, paths)
        })
        .collect();

    Ok((image_path, cache_entries))
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{} GB", bytes / (1024 * 1024 * 1024))
    } else {
        format!("{} MB", bytes / (1024 * 1024))
    }
}

/// Create a new sparse file (allocates no disk blocks until written).
/// Ask whether to erase and recreate the disk at a smaller size. Returns
/// `true` only on explicit confirmation. Without a TTY we can't ask, so we
/// return `false` (keep the larger disk) rather than destroy data silently.
/// The default selection is the non-destructive one, so an accidental Enter
/// never wipes the disk.
fn prompt_shrink_disk(current: u64, target: u64) -> anyhow::Result<bool> {
    if !cli::is_interactive() {
        return Ok(false);
    }
    let term = dialoguer::console::Term::stderr();
    let choice = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(format!(
            "Configured disk size {} is smaller than the current {}. \
             Shrinking erases all sandbox data on the disk.",
            format_size(target),
            format_size(current),
        ))
        .items([
            "Keep the current disk (no change)",
            "Erase and recreate at the smaller size (loses all data)",
        ])
        .default(0)
        .clear(true)
        .interact_on_opt(&term)?
        .unwrap_or(0);
    let _ = term.clear_last_lines(1);
    Ok(choice == 1)
}

fn create_sparse(path: &Path, size: u64) -> anyhow::Result<()> {
    let file = fs::File::create(path)?;
    file.set_len(size)?;
    Ok(())
}

/// Grow an existing sparse file to a larger size.
fn grow_sparse(path: &Path, size: u64) -> anyhow::Result<()> {
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(size)?;
    Ok(())
}
