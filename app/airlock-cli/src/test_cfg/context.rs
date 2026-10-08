use std::path::Path;
use std::sync::Arc;

use airlock_test_utils::{TempDir, temp_dir};

use crate::context::Context;
use crate::db::Db;
use crate::services::store::TokenStore;
use crate::settings::Settings;
use crate::vault::Vault;

/// The process context of a test: default settings, `vault`, and the
/// database in `home` (a temp dir the caller keeps).
pub fn test_context(home: &Path, vault: Vault) -> Context {
    Context {
        settings: Settings::load_from(home).unwrap(),
        vault,
        db: Db::open(&home.join(crate::db::DIR)).unwrap(),
    }
}

/// A database in a fresh temp home (kept alive by the returned guard).
pub fn test_db() -> (TempDir, Db) {
    let dir = temp_dir();
    let db = Db::open(&dir.path().join(crate::db::DIR)).unwrap();
    (dir, db)
}

/// The temp home of a [`test_store`] and its database.
pub struct StoreHome {
    _dir: TempDir,
    pub db: Db,
}

/// A token store in the database of a test context in a fresh temp home
/// (kept alive by the returned [`StoreHome`]).
pub fn test_store() -> (StoreHome, Arc<TokenStore>) {
    let (dir, db) = test_db();
    let store = TokenStore::new(db.clone(), &[42; 32]);
    (StoreHome { _dir: dir, db }, Arc::new(store))
}
