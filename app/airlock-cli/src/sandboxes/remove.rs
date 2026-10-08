//! Removal of a sandbox in the data directory.

use super::registry;
use crate::context::Context;
use crate::project::{self, IdleLock};

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
    anyhow::ensure!(registry::is_valid_id(id), "not a sandbox id: {id}");
    let dir = context.boxes_dir().join(id);
    let _lock = match project::lock_if_idle(&dir) {
        IdleLock::Running => anyhow::bail!("Sandbox is running, stop it first"),
        IdleLock::Held(file) => Some(file),
        IdleLock::Missing => None,
    };
    match std::fs::symlink_metadata(&dir) {
        // `remove_dir_all` does not follow a symlink at `dir`, but remove
        // only the link to make that clear.
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(&dir)?,
        Ok(_) => std::fs::remove_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("remove {}: {e}", dir.display()))?,
        Err(_) => {}
    }
    registry::unregister(&context.db, id).await?;
    Ok(())
}
