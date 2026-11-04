//! Disk of a sandbox.
//!
//! Makes, resizes and resets the disk of a sandbox, and tells if a sandbox
//! has a disk and which disk it is.

use crate::config::config_values::Disk;
use crate::messages;
use crate::util::{PinnedDir, VerifiedPath};
use crate::vm::disk;

/// Create the sandbox disk in `sandbox_dir`, or change it to the size in
/// `config` (`[disk] size`). Prints a line for each change. Use only under
/// the sandbox lock, while no VM runs. Set `copied` if the disk is a new
/// copy of a template disk.
pub fn ensure_disk(sandbox_dir: &PinnedDir, config: &Disk, copied: bool) -> anyhow::Result<()> {
    disk::ensure(sandbox_dir, config, copied)
}

/// Delete the sandbox disk in `sandbox_dir` (the stored rootfs changes and
/// the caches). The next boot creates and formats a new disk. Use only
/// under the sandbox lock, while no VM runs.
pub fn reset_disk(sandbox_dir: &PinnedDir) -> anyhow::Result<()> {
    sandbox_dir
        .remove(disk::DISK_FILE)
        .map_err(|e| anyhow::anyhow!(messages::project::remove_disk_failed(e)))?;
    sandbox_dir
        .remove(disk::DISK_ID_FILE)
        .map_err(|e| anyhow::anyhow!(messages::project::remove_disk_id_failed(e)))?;
    Ok(())
}

/// Check if the sandbox in `sandbox_dir` has a disk image. Also true for
/// an image from an older airlock without an identity file. Only a
/// regular file counts. The check does not open the file, so a planted
/// FIFO cannot block it.
pub fn has_disk(sandbox_dir: &VerifiedPath) -> bool {
    sandbox_dir
        .join(disk::DISK_FILE)
        .and_then(|path| path.symlink_metadata())
        .is_ok_and(|m| m.is_file())
}

/// Get the identity of the sandbox disk image: a random id that each new
/// image gets (see [`disk::read_id`]). `None` if there is no identity yet.
pub fn disk_id(sandbox_dir: &PinnedDir) -> Option<(u64, u64)> {
    disk::read_id(sandbox_dir)
}
