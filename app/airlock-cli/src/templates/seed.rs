//! New sandboxes from templates.
//!
//! Gives a new sandbox the disk, the install records, the install
//! snapshots, the CA and the image of a template. The disk is a
//! copy-on-write clone where the file system can do it. Otherwise it is a
//! normal copy that keeps the disk sparse. The new disk gets its own
//! identity, and the install records move to it.

use super::{CA_FILE, IMAGE_FILE, Template};
use crate::messages;
use crate::packs::install::snapshots::{SNAPSHOTS_CAP, SNAPSHOTS_FILE};
use crate::packs::install::state::{self as install_state, ReadState};
use crate::util::PinnedDir;
use crate::util::clone::clone_file;
use crate::util::verified_path::VerifiedPath;
use crate::vm::disk::{self, DISK_FILE};

/// Copy the template `template` into the new sandbox `sandbox`.
///
/// The sandbox must have no disk and no install records. After the copy,
/// the sandbox has a copy of the template disk with a new identity, the
/// install records of the template for that disk, the CA of the template,
/// and the template image. The image is optional: if the link fails, the
/// image step gets the image again.
/// Args:
///  - `template`: The template
///  - `sandbox`: Directory of the new sandbox. The caller holds its lock.
///  - `sandbox_dir`: Path of `sandbox`
///
/// Returns:
///   Error if the copy fails. Then the sandbox has no disk.
pub fn seed(
    template: &Template,
    sandbox: &PinnedDir,
    sandbox_dir: &VerifiedPath,
) -> anyhow::Result<()> {
    let src = template.dir.join(DISK_FILE)?;
    let dst = sandbox_dir.join(DISK_FILE)?;
    let copied = clone_file(&src, &dst).map_err(|e| {
        anyhow::anyhow!(messages::templates::copy_disk_failed(
            &template.name,
            &src,
            &dst,
            e
        ))
    });
    if let Err(e) = copied {
        let _ = sandbox.remove(DISK_FILE);
        return Err(e);
    }
    let result = seed_records(template, sandbox)
        .and_then(|()| copy_file(template, sandbox, SNAPSHOTS_FILE, SNAPSHOTS_CAP))
        .and_then(|()| copy_file(template, sandbox, CA_FILE, 1024 * 1024));
    if result.is_err() {
        unseed(sandbox_dir);
        return result;
    }
    // The image link is only a shortcut. Without it, the image step gets
    // the image again. The link source cannot be a symlink, because a
    // link of a symlink follows it on macOS.
    if let (Ok(src), Ok(dst)) = (template.dir.join(IMAGE_FILE), sandbox_dir.join(IMAGE_FILE)) {
        let _ = src.hard_link_to(&dst);
    }
    Ok(())
}

/// Give the copied disk in `sandbox` a new identity, and write the install
/// records of `template` for it.
fn seed_records(template: &Template, sandbox: &PinnedDir) -> anyhow::Result<()> {
    disk::write_id(sandbox)?;
    let source = PinnedDir::pin(template.dir.to_path()?)?;
    let mut state = match install_state::read(&source) {
        ReadState::Ok(state) => state,
        ReadState::Absent => return Ok(()),
        other => anyhow::bail!(messages::templates::install_records_invalid(
            &template.name,
            other
        )),
    };
    state.disk = disk::read_id(sandbox);
    install_state::write(sandbox, &state)
}

/// Copy the file `name` of `template` into `sandbox`, if the template has
/// it. The copy has mode 0600.
/// Args:
///  - `cap`: Maximum size of the file in bytes
// The pack installs can put the CA certificate into the CA bundles on the
// disk. A new CA would then not be in these bundles. The install snapshots
// show the changes of a pack since the install of the template.
fn copy_file(template: &Template, sandbox: &PinnedDir, name: &str, cap: u64) -> anyhow::Result<()> {
    let source = PinnedDir::pin(template.dir.to_path()?)?;
    if let Some(bytes) = source.read(name, cap)? {
        sandbox.write_atomic(name, &bytes, 0o600)?;
    }
    Ok(())
}

/// Remove the template disk, its install records and its install
/// snapshots from the sandbox `sandbox`. The image and the CA stay. The
/// next disk is empty, and all packs install again.
/// Returns:
///   Error if a remove fails.
pub fn drop_disk(sandbox: &PinnedDir) -> anyhow::Result<()> {
    crate::project::reset_disk(sandbox)?;
    sandbox.remove(install_state::STATE_FILE)?;
    sandbox.remove(SNAPSHOTS_FILE)?;
    Ok(())
}

/// Remove the files that [`seed`] made in the sandbox directory
/// `sandbox_dir`. Call this if the start fails after the copy. Then the
/// next start sees a new sandbox again.
pub fn unseed(sandbox_dir: &VerifiedPath) {
    for file in [
        DISK_FILE,
        disk::DISK_ID_FILE,
        install_state::STATE_FILE,
        SNAPSHOTS_FILE,
        IMAGE_FILE,
        CA_FILE,
    ] {
        if let Ok(path) = sandbox_dir.join(file) {
            let _ = path.remove_file();
        }
    }
}
