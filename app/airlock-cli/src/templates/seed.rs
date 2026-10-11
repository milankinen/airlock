//! New sandboxes from templates.
//!
//! Gives a new sandbox the disk, the install records, the CA and the image
//! of a template. The disk is a copy-on-write clone where the file system can
//! do it. Otherwise it is a normal copy that keeps the disk sparse. The new
//! disk gets its own identity, and the install records move to it.

use std::path::Path;

use super::{CA_FILE, IMAGE_FILE, Template};
use crate::messages;
use crate::packs::install::state::{self as install_state, ReadState};
use crate::util::PinnedDir;
use crate::util::clone::clone_file;
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
///
/// Returns:
///   Error if the copy fails. Then the sandbox has no disk.
pub fn seed(template: &Template, sandbox: &PinnedDir) -> anyhow::Result<()> {
    let src = template.dir.join(DISK_FILE);
    let dst = sandbox.path().join(DISK_FILE);
    let copied = clone_file(&src, &dst).map_err(|e| {
        anyhow::anyhow!(messages::templates::copy_disk_failed(
            &template.name,
            src.display(),
            dst.display(),
            e
        ))
    });
    if let Err(e) = copied {
        let _ = sandbox.remove(DISK_FILE);
        return Err(e);
    }
    let result = seed_records(template, sandbox).and_then(|()| seed_ca(template, sandbox));
    if result.is_err() {
        unseed(sandbox.path());
        return result;
    }
    // The image link is only a shortcut. Without it, the image step gets
    // the image again.
    let _ = std::fs::hard_link(
        template.dir.join(IMAGE_FILE),
        sandbox.path().join(IMAGE_FILE),
    );
    Ok(())
}

/// Give the copied disk in `sandbox` a new identity, and write the install
/// records of `template` for it.
fn seed_records(template: &Template, sandbox: &PinnedDir) -> anyhow::Result<()> {
    disk::write_id(sandbox.path())?;
    let source = PinnedDir::pin(&template.dir)?;
    let mut state = match install_state::read(&source) {
        ReadState::Ok(state) => state,
        ReadState::Absent => return Ok(()),
        other => anyhow::bail!(messages::templates::install_records_invalid(
            &template.name,
            other
        )),
    };
    state.disk = disk::read_id(sandbox.path());
    install_state::write(sandbox, &state)
}

/// Copy the CA of `template` into `sandbox`, if the template has one.
// The pack installs can put the CA certificate into the CA bundles on the
// disk. A new CA would then not be in these bundles.
fn seed_ca(template: &Template, sandbox: &PinnedDir) -> anyhow::Result<()> {
    let source = PinnedDir::pin(&template.dir)?;
    if let Some(bytes) = source.read(CA_FILE, 1024 * 1024)? {
        sandbox.write_atomic(CA_FILE, &bytes, 0o600)?;
    }
    Ok(())
}

/// Remove the files that [`seed`] made in the sandbox directory
/// `sandbox_dir`. Call this if the start fails after the copy. Then the
/// next start sees a new sandbox again.
pub fn unseed(sandbox_dir: &Path) {
    for file in [
        DISK_FILE,
        disk::DISK_ID_FILE,
        install_state::STATE_FILE,
        IMAGE_FILE,
        CA_FILE,
    ] {
        let _ = std::fs::remove_file(sandbox_dir.join(file));
    }
}
