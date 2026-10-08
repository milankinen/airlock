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
/// `main` creates it once with [`Context::load`] and gives it to the
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
    /// Load the context. The settings come from the user's airlock
    /// directory `~/.airlock`. The database is in the data directory of the
    /// settings, which also becomes the data directory of the process (see
    /// [`crate::cache::set_data_dir`]).
    /// Returns:
    ///   The context, or error if the settings file is malformed or the
    ///   database does not open. A missing settings file gives defaults.
    pub fn load() -> anyhow::Result<Self> {
        let settings = Settings::load_from(&Settings::dir()?)?;
        let vault = Vault::for_storage_type(settings.vault.storage);
        let data_dir = settings.data_dir()?;
        crate::cache::set_data_dir(data_dir.clone());
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
