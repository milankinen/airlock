//! Removal of a sandbox in the data directory.

use super::registry;
use crate::context::Context;
use crate::project::{self, IdleLock};
use crate::util::vfs;

/// Remove the sandbox `id` from the data directory and from the registry.
///
/// Holds the sandbox lock during the removal. Thus a parallel `airlock
/// start` cannot take the sandbox while it goes. Does not run the image
/// garbage collection.
/// Args:
///  - `context`: Process context, for the registry and the data directory
///  - `id`: Registry id. It must have the form of an id, thus it can never
///    name a path outside the sandboxes directory.
///
/// Returns:
///   Error if the id is not valid, the sandbox runs, or the removal fails.
pub async fn remove_box(context: &Context, id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        registry::is_valid_id(id),
        crate::messages::sandboxes::not_sandbox_id(id)
    );
    let dir = super::boxes_dir(context)?.join(id)?;
    let _lock = match project::lock_if_idle(&dir) {
        IdleLock::Running => anyhow::bail!(crate::messages::sandboxes::SANDBOX_RUNNING_STOP_FIRST),
        IdleLock::Held(file) => Some(file),
        IdleLock::Missing => None,
    };
    // A symlink at `dir` is removed, not followed.
    match vfs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            anyhow::bail!(crate::messages::sandboxes::remove_failed(dir, e))
        }
        _ => {}
    }
    registry::unregister(&context.db, id).await?;
    Ok(())
}
