//! Removal of a sandbox in the data directory.

use super::lock::{self, LockKind};
use super::registry;
use crate::context::Context;

/// Remove the sandbox `id` from the data directory and from the registry.
///
/// Holds the sandbox locks during the removal. Thus a parallel `airlock
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
        crate::messages::sandbox::not_sandbox_id(id)
    );
    let dir = context.data.sandboxes.join(id)?;
    // The same order as all lock users: the startup lock, then the run
    // lock. Both must be free.
    let mut held = Vec::new();
    let mut made = Vec::new();
    let is_dir = dir
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_dir());
    let running = || anyhow::anyhow!(crate::messages::sandbox::SANDBOX_RUNNING_STOP_FIRST);
    for kind in [LockKind::Startup, LockKind::Run] {
        match lock::probe(&dir, kind) {
            lock::IdleLock::Busy => return Err(running()),
            lock::IdleLock::Held(file) => held.push(file),
            // A missing lock file in the directory is made and taken. Thus
            // a `start` that comes now waits for the removal.
            lock::IdleLock::Missing if is_dir => {
                made.push(lock::try_lock(&dir, kind)?.ok_or_else(running)?);
            }
            lock::IdleLock::Missing => {}
        }
    }
    // A symlink at `dir` is removed, not followed.
    match dir.remove_dir_all() {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            anyhow::bail!(crate::messages::sandbox::remove_failed(dir, e))
        }
        _ => {}
    }
    registry::unregister(&context.db, id).await?;
    drop((held, made));
    Ok(())
}

impl super::Sandbox {
    /// Remove the sandbox from the data directory and from the registry
    /// (see [`remove_box`]).
    /// Returns:
    ///   Error if the sandbox runs or the removal fails.
    pub async fn remove(&self, context: &Context) -> anyhow::Result<()> {
        remove_box(context, &self.id).await
    }
}
