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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::HOME_LOCK;

    #[test]
    fn lock_path_is_under_the_airlock_home() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old_home = std::env::var_os("HOME");
        let home =
            std::env::temp_dir().join(format!("airlock-keyring-home-{}", std::process::id()));
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let path = KeyringStorage.lock_path().unwrap();
        unsafe {
            match old_home {
                Some(old) => std::env::set_var("HOME", old),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(path, Some(home.join(".airlock/vault.keyring.lock")));
    }
}
