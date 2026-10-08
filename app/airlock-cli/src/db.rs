//! The airlock database.
//!
//! A key-value store in the airlock home directory. All airlock processes of
//! the user share it. It contains named databases for different purposes.
//! This module does not know the contents of any of them.

use std::path::Path;

use anyhow::{Context as _, bail};
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};

/// Directory of the database in the airlock home directory.
pub const DIR: &str = "db";

/// Maximum size of the environment, for all databases together. LMDB
/// reserves only the address space. The file grows with the data.
const MAP_SIZE: usize = 64 * 1024 * 1024;

/// Maximum number of named databases in the environment.
const MAX_DBS: u32 = 32;

/// The open database: one LMDB environment (through `heed`).
///
/// Clones are cheap and share one environment. The process has one [`Db`].
/// [`crate::context::Context::load`] opens it, and all users clone it from
/// there. LMDB allows one environment handle per path in a process, and
/// `heed` refuses a second open of the same directory.
///
/// LMDB serializes write transactions across processes with a lock in
/// `lock.mdb`. A writer waits for the other, so there is no `Busy` error to
/// retry. A transaction belongs to one thread, and a waiting writer blocks
/// its thread. Thus [`Db::read`] and [`Db::write`] run the full transaction
/// inside [`tokio::task::spawn_blocking`]. Keep transactions short. Never
/// hold one across an `await` or network I/O.
#[derive(Clone)]
pub struct Db(Env<WithoutTls>);

impl Db {
    /// Open the database in the directory `dir`. Creates the directory
    /// (mode 0700, LMDB files 0600) if it does not exist. Blocks.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        open_env(dir).map(Self)
    }

    /// Get the handle of the database `name`. Creates the database if it
    /// does not exist.
    ///
    /// Name a new database `<purpose>` or `<purpose>.<name>` (as `services`).
    /// Use the handle in the transactions of [`Db::read`] and [`Db::write`].
    /// Keep the records small: all databases share [`MAP_SIZE`], and there is
    /// space for [`MAX_DBS`] databases.
    pub async fn database<K, V>(&self, name: &str) -> anyhow::Result<Database<K, V>>
    where
        K: Send + 'static,
        V: Send + 'static,
    {
        let db = self.clone();
        let name = name.to_string();
        blocking(move || {
            let mut txn = db.0.write_txn()?;
            let database =
                db.0.create_database(&mut txn, Some(&name))
                    .with_context(|| format!("create the database {name}"))?;
            txn.commit()?;
            Ok(database)
        })
        .await
    }

    /// Delete all records of the database `name`, if it exists. Use it for a
    /// database that is no longer in use. The name stays, because `heed`
    /// cannot delete a named database.
    /// Returns:
    ///   `true` if the database had records.
    pub async fn empty_database(&self, name: &str) -> anyhow::Result<bool> {
        let db = self.clone();
        let name = name.to_string();
        blocking(move || {
            let mut txn = db.0.write_txn()?;
            let Some(database) =
                db.0.open_database::<heed::types::Bytes, heed::types::Bytes>(&txn, Some(&name))?
            else {
                return Ok(false);
            };
            let had = !database.is_empty(&txn)?;
            if had {
                database.clear(&mut txn)?;
            }
            txn.commit()?;
            Ok(had)
        })
        .await
    }

    /// Run `body` in a read transaction.
    /// Returns:
    ///   The result of `body`.
    pub async fn read<T: Send + 'static>(
        &self,
        body: impl FnOnce(&RoTxn<'_, WithoutTls>) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let db = self.clone();
        blocking(move || {
            let txn = db.0.read_txn().context("begin a database transaction")?;
            body(&txn)
        })
        .await
    }

    /// Run `body` in a write transaction. Commits if `body` succeeds, and
    /// aborts if it fails. Waits while another process writes.
    /// Returns:
    ///   The result of `body`.
    pub async fn write<T: Send + 'static>(
        &self,
        body: impl FnOnce(&mut RwTxn<'_>) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let db = self.clone();
        blocking(move || {
            let mut txn = db.0.write_txn().context("begin a database transaction")?;
            let value = body(&mut txn)?;
            txn.commit().context("commit a database transaction")?;
            Ok(value)
        })
        .await
    }

    /// Whether the database `name` exists. Blocks.
    #[cfg(test)]
    pub fn has_database(&self, name: &str) -> bool {
        let txn = self.0.read_txn().unwrap();
        self.0
            .open_database::<heed::types::Bytes, heed::types::Bytes>(&txn, Some(name))
            .unwrap()
            .is_some()
    }

    /// The number of records of the database `name` (0 when it does not
    /// exist). Blocks.
    #[cfg(test)]
    pub fn database_len(&self, name: &str) -> u64 {
        let txn = self.0.read_txn().unwrap();
        self.0
            .open_database::<heed::types::Bytes, heed::types::Bytes>(&txn, Some(name))
            .unwrap()
            .map_or(0, |db| db.len(&txn).unwrap())
    }
}

/// Run `op` on a blocking thread. LMDB transactions block and belong to
/// their thread.
async fn blocking<T: Send + 'static>(
    op: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(op)
        .await
        .context("database task")?
}

/// Open the environment in `dir`. Creates the directory (mode 0700, LMDB
/// files 0600) if it does not exist. Blocks.
fn open_env(dir: &Path) -> anyhow::Result<Env<WithoutTls>> {
    prepare_dir(dir)?;
    let mut options = EnvOpenOptions::new().read_txn_without_tls();
    options.map_size(MAP_SIZE).max_dbs(MAX_DBS);
    // SAFETY: the memory map is sound only while LMDB is the only writer of
    // the files. Only airlock writes them, always through LMDB and this
    // function. A process opens the environment once (one `Db` in the
    // context, and `heed` refuses a second open of the same path). The
    // directory is private to the user and is in the home directory, not on
    // a network file system.
    let env = unsafe { options.open(dir) }
        .with_context(|| format!("open the database {}", dir.display()))?;
    // Reader slots of processes that died during a read keep old pages
    // in use. Clear them.
    env.clear_stale_readers()
        .context("clear stale database readers")?;
    Ok(env)
}

/// Create the environment directory (mode 0700) before LMDB creates its
/// files (0600) in it. If the directory exists with wider modes, restrict it
/// to 0700.
fn prepare_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {} to its owner", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
