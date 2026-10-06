//! The process-wide context: the user's settings, the vault and the
//! database.
//!
//! Built once in `main` ([`Context::load`]) and handed to the commands;
//! a [`crate::project::Project`] holds it, and the modules of a project
//! take what they need from there. Cheap to clone: the vault and the
//! database are shared handles.

use crate::db::{self, Db};
use crate::settings::Settings;
use crate::vault::Vault;

#[derive(Clone)]
pub struct Context {
    /// Application settings from `~/.airlock/settings.*`.
    pub settings: Settings,
    /// The one vault of the process. The backend is selected by
    /// `settings.vault`; for `disabled` it is an inert no-op so no callee
    /// has to special-case it. Opens on first use.
    pub vault: Vault,
    /// The airlock database `~/.airlock/db/` (see [`crate::db`]), open.
    pub db: Db,
}

impl Context {
    /// The context of the user's airlock directory `~/.airlock`. Absent
    /// settings file → defaults; a malformed one is an error, so the user
    /// does not silently get defaults. Opens the database (created when
    /// absent); the vault opens on first use.
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
