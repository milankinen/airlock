//! Disabled vault storage.
//!
//! Used for `settings.vault.storage = "disabled"`. Stores nothing.

use super::Storage;

/// Storage backend that stores nothing. Reads return an empty vault and
/// writes are dropped.
pub struct DisabledStorage;

impl Storage for DisabledStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
