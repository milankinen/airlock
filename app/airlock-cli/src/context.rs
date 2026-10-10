//! Process-wide context.
//!
//! Holds the user's settings, the vault, the database and the location of
//! the airlock data. The commands and the project get these shared resources
//! from the context.

use std::path::PathBuf;

use crate::db::{self, Db};
use crate::settings::Settings;
use crate::vault::Vault;

/// User settings, vault and database of the process.
///
/// `main` creates it once with [`Context::new`] and gives it to the
/// commands. A [`crate::project::Project`] holds it, and the project modules
/// take what they need from there. Clones are cheap because the vault and the
/// database are shared handles.
#[derive(Clone)]
pub struct Context {
    /// Application settings from `~/.airlock/settings.*`.
    pub settings: Settings,
    /// The single vault of the process. `settings.vault` selects the backend.
    /// For `disabled`, the vault does nothing, so callers do not need a
    /// special case for it. The vault opens on first use.
    pub vault: Vault,
    /// The open airlock database `<data_dir>/db/` (see [`crate::db`]).
    pub db: Db,
    /// The airlock data directory (see [`crate::cache`]). The sandboxes
    /// that are not in their project are in `<data_dir>/boxes/`.
    pub data_dir: PathBuf,
}

impl Context {
    /// Make the context of the process.
    /// Args:
    ///  - `settings`: User settings (see [`Settings::load`])
    ///  - `data_dir`: Airlock data directory, after the move of the cache
    ///    of an older version (see [`crate::cache::migrate_legacy_cache`]).
    ///    Created with mode 0700 if it does not exist.
    ///
    /// Returns:
    ///   The context, or error if airlock cannot make the data directory or
    ///   open the database.
    pub fn new(settings: Settings, data_dir: PathBuf) -> anyhow::Result<Self> {
        crate::cache::create_private_dir(&data_dir)?;
        let vault = Vault::for_storage_type(settings.vault.storage);
        let db = Db::open(&data_dir.join(db::DIR))?;
        Ok(Self {
            settings,
            vault,
            db,
            data_dir,
        })
    }

    /// Get the directory of the sandboxes that are not in their project.
    pub fn boxes_dir(&self) -> PathBuf {
        self.data_dir.join(crate::sandboxes::BOXES_DIR)
    }
}
