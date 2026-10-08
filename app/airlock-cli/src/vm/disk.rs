//! Sandbox disk.
//!
//! Creates and resizes the persistent disk of a sandbox, and identifies each
//! disk. The disk keeps all changes that the sandbox makes to the container
//! file system, and the contents of the named caches.

use std::fs;
use std::path::{Path, PathBuf};

use crate::cli;
use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::cli::prompt::style::Tone;
use crate::config::config_values::Disk;

/// Default disk size (10 GB) for the overlay upper layer and cache dirs.
const DEFAULT_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The disk image file in the sandbox directory.
pub const DISK_FILE: &str = "disk.img";
/// The identity file of the disk image, next to the image. Contains a random
/// id that changes with each new image ([`read_id`]).
pub const DISK_ID_FILE: &str = "disk.id";

/// Named cache entry: `(name, enabled, expanded_container_paths)`.
pub type CacheEntry = (String, bool, Vec<String>);

/// Make the disk image in `cache_dir` match the configured size. Creates
/// the image, grows it, or (if the user confirms) makes it again smaller.
/// Prints a line for each change.
pub fn ensure(cache_dir: &Path, config: &Disk) -> anyhow::Result<()> {
    let image_path = cache_dir.join(DISK_FILE);

    let bytes = (config.size.0 + 511) & !511;
    let bytes = if bytes > 0 {
        bytes
    } else {
        (DEFAULT_DISK_BYTES + 511) & !511
    };

    if image_path.exists() {
        let current_size = fs::metadata(&image_path)?.len();
        if current_size > bytes {
            // The disk holds the overlay upper layer (all sandbox writes) and
            // the named caches. A smaller disk destroys this data, so do it
            // only if the user confirms. Then make a new, empty image at the
            // smaller size. If the user declines or there is no TTY, keep the
            // larger disk.
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
        // A disk from before identity files existed.
        if read_id(cache_dir).is_none() {
            write_id(cache_dir)?;
        }
    } else {
        create_sparse(&image_path, bytes)?;
        cli::log!(
            "  {} disk created {}",
            cli::check(),
            cli::dim(&format_size(bytes))
        );
    }
    Ok(())
}

/// Get the disk image and the cache entries for a VM boot. Creates the disk
/// image if it is missing. `airlock start` sets its size earlier, when it
/// prepares the sandbox ([`ensure`]).
/// Args:
///  - `cache_dir`: Sandbox directory that contains the disk image
///  - `config`: Disk config
///  - `container_home`: Guest home for `~` expansion of cache paths
///  - `cwd`: Base directory for relative cache paths.
///
/// Returns:
///   The disk image path and all cache entries (enabled and disabled).
pub fn prepare(
    cache_dir: &Path,
    config: &Disk,
    container_home: &str,
    cwd: &Path,
) -> anyhow::Result<(PathBuf, Vec<CacheEntry>)> {
    let image_path = cache_dir.join(DISK_FILE);
    if !image_path.exists() {
        ensure(cache_dir, config)?;
    }

    let container_home = PathBuf::from(container_home);
    // Include all entries (enabled and disabled), so the supervisor knows
    // all declared names. It removes the disk dirs of names that are not in
    // this list, and does not mount disabled entries.
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

/// Format a byte count as whole GB or MB.
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{} GB", bytes / (1024 * 1024 * 1024))
    } else {
        format!("{} MB", bytes / (1024 * 1024))
    }
}

/// Ask the user if airlock erases the disk and makes it again at a smaller
/// size.
/// Returns:
///   `true` only if the user confirms. Without a TTY, `false` (keep the
///   larger disk), so no data is destroyed silently.
///
/// The default selection keeps the disk, so an accidental Enter never erases
/// it. Esc also keeps it.
fn prompt_shrink_disk(current: u64, target: u64) -> anyhow::Result<bool> {
    if !cli::is_interactive() {
        return Ok(false);
    }
    let title = format!(
        "Configured disk size {} is smaller than the current {}",
        format_size(target),
        format_size(current),
    );
    let erase = format!("erase and recreate at {}", format_size(target));
    let question = Choose {
        title: &title,
        notes: &["Shrinking erases all sandbox data on the disk."],
        choices: &[
            Choice {
                label: "keep the current disk",
                tone: Tone::Plain,
            },
            Choice {
                label: &erase,
                tone: Tone::Danger,
            },
        ],
        default: 0,
        report: false,
    };
    match question.ask() {
        Ok(choice) => Ok(choice == Some(1)),
        Err(PromptError::NotInteractive) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Create a new disk image and give it a new identity. The old identity is
/// removed first, so a crash never leaves the new image with the old
/// identity.
fn create_sparse(path: &Path, size: u64) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    remove_id(dir)?;
    let file = fs::File::create(path)?;
    file.set_len(size)?;
    write_id(dir)
}

/// Read the identity of the disk image in `dir`.
///
/// Each new image (first boot, `airlock rm`, reset, smaller size, manual
/// delete) gets a new random identity. Thus a new disk never looks like the
/// old one, also if it has the same inode.
/// Returns:
///   The identity, or `None` if there is no image or no valid identity file.
pub fn read_id(dir: &Path) -> Option<(u64, u64)> {
    if !dir.join(DISK_FILE).is_file() {
        return None;
    }
    let text = fs::read_to_string(dir.join(DISK_ID_FILE)).ok()?;
    parse_id(text.trim())
}

/// Parse a 32-digit hex identity into two `u64` halves.
fn parse_id(text: &str) -> Option<(u64, u64)> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let id = u128::from_str_radix(text, 16).ok()?;
    Some(((id >> 64) as u64, id as u64))
}

/// Write a new random identity for the disk image in `dir`.
fn write_id(dir: &Path) -> anyhow::Result<()> {
    use rand::TryRng;
    let mut bytes = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow::anyhow!("random disk id: {e}"))?;
    fs::write(
        dir.join(DISK_ID_FILE),
        format!("{:032x}\n", u128::from_be_bytes(bytes)),
    )?;
    Ok(())
}

/// Remove the identity file in `dir`, if any.
fn remove_id(dir: &Path) -> std::io::Result<()> {
    match fs::remove_file(dir.join(DISK_ID_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Grow an existing sparse file to a larger size.
fn grow_sparse(path: &Path, size: u64) -> anyhow::Result<()> {
    let file = fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(size)?;
    Ok(())
}
