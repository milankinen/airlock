//! Inert backend. Reads return empty, writes are dropped. Used when
//! `settings.vault = "disabled"` so `airlock secret` can refuse to run
//! without a separate "is the vault on" check elsewhere.

use super::Storage;

pub struct DisabledStorage;

impl Storage for DisabledStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
