//! The airlock database: `~/.airlock/db/`.
//!
//! One LMDB environment (through `heed`) shared by every airlock process
//! of the user. It hosts named databases for different purposes; this
//! module knows none of them. [`Db::open`] opens (and creates) the
//! environment; the process has one [`Db`], opened by
//! [`crate::context::Context::load`] and cloned from there.
//!
//! ## Adding a database
//!
//! Name it `<purpose>` or `<purpose>.<name>` (as `services`), get its handle with
//! [`Db::database`] (created when missing) and use it in the transactions
//! of [`Db::read`] and [`Db::write`]. Keep the records small: the
//! map is [`MAP_SIZE`] for all databases together, and the environment
//! has room for [`MAX_DBS`] databases.
//!
//! ## Transactions
//!
//! LMDB serializes write transactions across processes with a lock in
//! `lock.mdb`: a writer waits for the other, so there is no `Busy` error
//! to retry. A transaction belongs to one thread, and a waiting writer
//! blocks its thread: [`Db::read`] and [`Db::write`] run the whole
//! transaction inside [`tokio::task::spawn_blocking`]. Keep transactions
//! short; never hold one across an `await` or network I/O.
//!
//! LMDB allows one environment handle per path in a process: clone the
//! [`Db`] of the context instead of making another on the same directory
//! (`heed` refuses the second open).

use std::path::Path;

use anyhow::{Context as _, bail};
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};

/// Directory of the database in the airlock home directory.
pub const DIR: &str = "db";

/// Upper bound of the environment's size, for all databases together.
/// LMDB reserves the address space only; the file grows with the data.
const MAP_SIZE: usize = 64 * 1024 * 1024;

/// The most named databases the environment holds.
const MAX_DBS: u32 = 32;

/// The open database. Cheap to clone; the clones share one environment.
#[derive(Clone)]
pub struct Db(Env<WithoutTls>);

impl Db {
    /// Open the database in the directory `dir`, created (0700; LMDB's
    /// files 0600) when absent. Blocks.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        open_env(dir).map(Self)
    }

    /// The database `name`, created when missing.
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

    /// Delete every record of the database `name`, if it exists: `heed`
    /// cannot delete a named database, so a database no longer in use is
    /// emptied (its name stays). Returns whether it had records.
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

    /// Run `body` in a write transaction: commit on success, abort on
    /// error. Waits while another process writes.
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

/// Run `op` on a blocking thread: LMDB transactions block and belong to
/// their thread.
async fn blocking<T: Send + 'static>(
    op: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(op)
        .await
        .context("database task")?
}

/// Open the environment in `dir`, created (0700; LMDB's files 0600) when
/// absent. Blocks.
fn open_env(dir: &Path) -> anyhow::Result<Env<WithoutTls>> {
    prepare_dir(dir)?;
    let mut options = EnvOpenOptions::new().read_txn_without_tls();
    options.map_size(MAP_SIZE).max_dbs(MAX_DBS);
    // SAFETY: the memory map is only sound while nothing but LMDB changes
    // the files. Only airlock writes them, always through LMDB and this
    // function; a process opens the environment once (one `Db` in the
    // context, and `heed` refuses a second open of the same path); the
    // directory is private to the user and lives in the home directory,
    // not on a network file system.
    let env = unsafe { options.open(dir) }
        .with_context(|| format!("open the database {}", dir.display()))?;
    // Reader slots of processes that died mid-read would pin old pages.
    env.clear_stale_readers()
        .context("clear stale database readers")?;
    Ok(env)
}

/// Create the environment directory (0700) before LMDB creates its files
/// (0600) in it; restrict a directory that exists with wider modes.
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
