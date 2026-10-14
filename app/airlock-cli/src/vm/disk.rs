//! Sandbox disk.
//!
//! Creates and resizes the persistent disk of a sandbox, and identifies each
//! disk. The disk keeps all changes that the sandbox makes to the container
//! file system, and the contents of the named caches.

use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::config::config_values::Disk;
use crate::util::{PinnedDir, VerifiedPath};
use crate::vm::mount;
use crate::{cli, messages};

/// Default disk size (10 GB) for the overlay upper layer and cache dirs.
const DEFAULT_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The disk image file in the sandbox directory.
pub const DISK_FILE: &str = "disk.img";
/// The identity file of the disk image, next to the image. Contains a random
/// id that changes with each new image ([`read_id`]).
pub const DISK_ID_FILE: &str = "disk.id";

/// Size limit of the identity file. Defense in depth: only airlock and the
/// old sandbox import write the file.
pub(crate) const DISK_ID_CAP: u64 = 64;

/// Permission bits of the disk image and its identity file.
const DISK_MODE: u32 = 0o600;

/// Named cache entry: `(name, enabled, expanded_container_paths)`.
pub type CacheEntry = (String, bool, Vec<String>);

/// Make the disk image in `cache_dir` match the configured size. Creates
/// the image, grows it, or (if the user confirms) makes it again smaller.
/// Prints a line for each change.
/// Args:
///  - `cache_dir`: Sandbox directory that contains the disk image
///  - `config`: Disk config
///  - `copied`: True if the disk image is a new copy of a template disk.
///    Then a line tells about the copy and its size.
pub fn ensure(cache_dir: &PinnedDir, config: &Disk, copied: bool) -> anyhow::Result<()> {
    let bytes = (config.size.0 + 511) & !511;
    let bytes = if bytes > 0 {
        bytes
    } else {
        (DEFAULT_DISK_BYTES + 511) & !511
    };

    if cache_dir.ino(DISK_FILE).is_some() {
        // The open refuses a symlink and a file that is not regular.
        let image = cache_dir.open_read_write(DISK_FILE, DISK_MODE)?;
        let current_size = image.metadata()?.len();
        if current_size > bytes {
            // The disk holds the overlay upper layer (all sandbox writes) and
            // the named caches. A smaller disk destroys this data, so do it
            // only if the user confirms. Then make a new, empty image at the
            // smaller size. If the user declines or there is no TTY, keep the
            // larger disk.
            if prompt_shrink_disk(current_size, bytes)? {
                drop(image);
                create_sparse(cache_dir, bytes)?;
                cli::log!(
                    "  {} {}",
                    cli::check(),
                    messages::vm::disk_recreated(cli::dim(&format_size(bytes)))
                );
            } else {
                cli::log!(
                    "  {} {}",
                    cli::yellow("!"),
                    messages::vm::disk_kept(
                        cli::dim(&format_size(current_size)),
                        cli::dim(&format_size(bytes))
                    )
                );
            }
        } else {
            let grown = current_size < bytes;
            if grown {
                image.set_len(bytes)?;
            }
            // For a template copy, one line tells about the copy and the
            // final size.
            if copied {
                cli::log!(
                    "  {} {}",
                    cli::check(),
                    messages::vm::disk_copied(cli::dim(&format_size(bytes)))
                );
            } else if grown {
                cli::log!(
                    "  {} {}",
                    cli::check(),
                    messages::vm::disk_grown(cli::dim(&format_size(bytes)))
                );
            }
        }
        // A disk from before identity files existed.
        if read_id(cache_dir).is_none() {
            write_id(cache_dir)?;
        }
    } else {
        create_sparse(cache_dir, bytes)?;
        cli::log!(
            "  {} {}",
            cli::check(),
            messages::vm::disk_created(cli::dim(&format_size(bytes)))
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
///  - `cwd`: Project directory, base for relative cache paths
///
/// Returns:
///   The disk image path and all cache entries (enabled and disabled).
pub fn prepare(
    cache_dir: &VerifiedPath,
    config: &Disk,
    container_home: &str,
    cwd: &str,
) -> anyhow::Result<(VerifiedPath, Vec<CacheEntry>)> {
    let image_path = cache_dir.join(DISK_FILE)?;
    match image_path.metadata() {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure(&PinnedDir::pin(cache_dir.to_path()?)?, config, false)?;
        }
        Err(e) => return Err(e.into()),
    }

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
                .map(|p| mount::join_str(cwd, &mount::expand_tilde_str(p, container_home)))
                .collect();
            (name.clone(), m.enabled, paths)
        })
        .collect();

    Ok((image_path, cache_entries))
}

/// Format a byte count as whole GB or MB.
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        messages::vm::size_gb(bytes / (1024 * 1024 * 1024))
    } else {
        messages::vm::size_mb(bytes / (1024 * 1024))
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
    let title = messages::vm::disk_shrink_title(format_size(target), format_size(current));
    let erase = messages::vm::disk_shrink_erase(format_size(target));
    let question = Choose {
        title: &title,
        notes: &[messages::vm::DISK_SHRINK_NOTE],
        choices: &[
            Choice {
                label: messages::vm::DISK_SHRINK_KEEP,
            },
            Choice { label: &erase },
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

/// Create a new disk image in `dir` and give it a new identity. The old
/// identity is removed first, so a crash never leaves the new image with
/// the old identity.
fn create_sparse(dir: &PinnedDir, size: u64) -> anyhow::Result<()> {
    remove_id(dir)?;
    // Remove the old image, so that the open makes a new empty file.
    dir.remove(DISK_FILE)?;
    let file = dir.open_read_write(DISK_FILE, DISK_MODE)?;
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
pub fn read_id(dir: &PinnedDir) -> Option<(u64, u64)> {
    dir.ino(DISK_FILE)?;
    let bytes = dir.read(DISK_ID_FILE, DISK_ID_CAP).ok()??;
    parse_id(std::str::from_utf8(&bytes).ok()?.trim())
}

/// Parse a 32-digit hex identity into two `u64` halves.
pub(crate) fn parse_id(text: &str) -> Option<(u64, u64)> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let id = u128::from_str_radix(text, 16).ok()?;
    Some(((id >> 64) as u64, id as u64))
}

/// Write a new random identity for the disk image in `dir`.
pub(crate) fn write_id(dir: &PinnedDir) -> anyhow::Result<()> {
    use rand::TryRng;
    let mut bytes = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow::anyhow!(messages::vm::disk_random_id_failed(e)))?;
    let text = format!("{:032x}\n", u128::from_be_bytes(bytes));
    dir.write_atomic(DISK_ID_FILE, text.as_bytes(), DISK_MODE)?;
    Ok(())
}

/// Remove the identity file in `dir`, if any.
fn remove_id(dir: &PinnedDir) -> std::io::Result<()> {
    dir.remove(DISK_ID_FILE).map(|_| ())
}

#[cfg(test)]
mod tests {
    //! Tests for the symlink safety of the sandbox disk.

    use super::*;
    use crate::test_cfg::{resolve_project_toml, temp_dir};

    /// Test that the disk setup does not follow a symlink at the disk image.
    /// A followed link could make the host change the size of a host file.
    ///   1. Put a symlink to a host file at the disk image name
    ///   2. Make the disk
    ///   3. Check that it fails and the host file did not change
    #[test]
    fn disk_setup_does_not_follow_planted_link() {
        let tmp = temp_dir();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"data").unwrap();
        let sandbox = tmp.path().join("sandbox");
        std::fs::create_dir(&sandbox).unwrap();
        std::os::unix::fs::symlink(&victim, sandbox.join(DISK_FILE)).unwrap();
        let config = resolve_project_toml("").unwrap().values.disk;

        ensure(&PinnedDir::pin(&sandbox).unwrap(), &config, false).unwrap_err();
        assert_eq!(std::fs::read(&victim).unwrap(), b"data");
    }
}
