//! Helpers that make a process context, a database and a token store in a
//! temporary home.

use std::path::Path;
use std::sync::Arc;

use airlock_test_utils::{TempDir, temp_dir};

use crate::context::Context;
use crate::db::Db;
use crate::services::store::TokenStore;
use crate::settings::Settings;
use crate::vault::Vault;

/// The process context of a test: the settings in `home` (defaults if
/// there is no file), `vault`, and `home` as the data directory with the
/// database in it. The caller keeps the `home` directory.
pub fn test_context(home: &Path, vault: Vault) -> Context {
    Context {
        settings: Settings::load_from(home).unwrap(),
        vault,
        db: Db::open(&home.join(crate::db::DIR)).unwrap(),
        data_dir: home.to_path_buf(),
    }
}

/// A database in a new temporary home. The database stays while the
/// returned directory exists.
pub fn test_db() -> (TempDir, Db) {
    let dir = temp_dir();
    let db = Db::open(&dir.path().join(crate::db::DIR)).unwrap();
    (dir, db)
}

/// The temporary home of a [`test_store`] and its database.
pub struct StoreHome {
    _dir: TempDir,
    /// The database of the token store.
    pub db: Db,
}

/// A token store in a database in a new temporary home, with a fixed key.
/// The store stays while the returned [`StoreHome`] exists.
pub fn test_store() -> (StoreHome, Arc<TokenStore>) {
    let (dir, db) = test_db();
    let store = TokenStore::new(db.clone(), &[42; 32]);
    (StoreHome { _dir: dir, db }, Arc::new(store))
}
