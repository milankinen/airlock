//! System keychain / Secret Service backend. Stores the vault blob as
//! a single secret under `airlock-vault / default`. On macOS first use
//! triggers the Keychain unlock prompt; on Linux it relies on the
//! Secret Service being available (GNOME Keyring, KeePassXC, …).
//!
//! The keychain has no compare-and-swap, so writers from different
//! processes serialize on the lock file `~/.airlock/vault.keyring.lock`.

use std::path::PathBuf;

use anyhow::{Context, anyhow};

use super::Storage;
use crate::settings::Settings;

const KEYRING_SERVICE: &str = "airlock-vault";
const KEYRING_ACCOUNT: &str = "default";

pub struct KeyringStorage;

impl Storage for KeyringStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        match keyring_entry()?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!("read airlock vault from keyring: {e}")),
        }
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        keyring_entry()?
            .set_password(data)
            .context("write airlock vault to keyring")
    }

    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        let dir = Settings::dir().context("find the airlock directory for the vault lock")?;
        Ok(Some(dir.join("vault.keyring.lock")))
    }
}

fn keyring_entry() -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("construct airlock keyring entry")
}
