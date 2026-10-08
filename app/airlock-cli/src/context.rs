//! Process-wide context.
//!
//! Holds the user's settings, the vault and the database. The commands and the
//! project get these shared resources from the context.

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
    /// The open airlock database `~/.airlock/db/` (see [`crate::db`]).
    pub db: Db,
}

impl Context {
    /// Load the context from the user's airlock directory `~/.airlock`.
    /// Returns:
    ///   The context, or error if the settings file is malformed or the
    ///   database does not open. A missing settings file gives defaults.
    pub fn load() -> anyhow::Result<Self> {
        let dir = Settings::dir()?;
        let settings = Settings::load_from(&dir)?;
        let vault = Vault::for_storage_type(settings.vault.storage);
        let db = Db::open(&dir.join(db::DIR))?;
        Ok(Self {
            settings,
            vault,
            db,
        })
    }
}
