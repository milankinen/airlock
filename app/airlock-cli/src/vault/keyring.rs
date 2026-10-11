//! System keychain vault storage.
//!
//! Used for `settings.vault.storage = "keyring"`. Keeps the vault in the system
//! keychain or Secret Service.

use std::path::PathBuf;

use anyhow::{Context, anyhow};

use super::Storage;
use crate::messages;
use crate::settings::Settings;

const KEYRING_SERVICE: &str = "airlock-vault";
const KEYRING_ACCOUNT: &str = "default";

/// Storage backend that keeps the vault blob as one secret in the system
/// keychain, under `airlock-vault / default`.
///
/// On macOS, the first use shows the Keychain unlock prompt. On Linux, a
/// Secret Service must be available (GNOME Keyring, KeePassXC, ...).
pub struct KeyringStorage;

impl Storage for KeyringStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        match keyring_entry()?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!(messages::vault::read_vault_from_keyring(e))),
        }
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        keyring_entry()?
            .set_password(data)
            .context(messages::vault::WRITE_VAULT_TO_KEYRING)
    }

    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        // The keychain has no compare-and-swap, so writers from different
        // processes serialize on a lock file.
        let dir = Settings::dir().context(messages::vault::FIND_VAULT_LOCK_DIR)?;
        Ok(Some(dir.join("vault.keyring.lock")))
    }
}

/// Open the keychain entry of the vault.
fn keyring_entry() -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .context(messages::vault::CONSTRUCT_KEYRING_ENTRY)
}
