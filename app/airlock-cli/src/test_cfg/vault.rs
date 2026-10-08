//! Vault backends, vaults and passphrase sources for tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::vault::{PassphraseSource, Storage, Vault, VaultStorageType};

/// A vault backend in memory. Clones share one blob, as two processes on
/// one backend do. The default backend has no vault lock.
#[derive(Clone, Default)]
pub struct MemoryStorage {
    blob: Arc<Mutex<Option<String>>>,
    lock: Option<PathBuf>,
}

impl MemoryStorage {
    /// A backend that takes the cross-process vault lock at `lock`.
    pub fn locked(lock: PathBuf) -> Self {
        Self {
            blob: Arc::default(),
            lock: Some(lock),
        }
    }

    /// The stored blob.
    pub fn blob(&self) -> Option<String> {
        self.blob.lock().clone()
    }
}

impl Storage for MemoryStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(self.blob())
    }
    fn store(&self, data: &str) -> anyhow::Result<()> {
        *self.blob.lock() = Some(data.to_string());
        Ok(())
    }
    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(self.lock.clone())
    }
}

/// A vault over `storage` with the host env `env`.
pub fn vault_with(storage: impl Storage, env: &[(&str, &str)]) -> Vault {
    Vault::new_with(
        Box::new(storage),
        env.iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<HashMap<_, _>>(),
        VaultStorageType::File,
    )
}

/// A passphrase source that always gives the same passphrase.
pub struct FixedPassphrase(pub &'static str);

impl PassphraseSource for FixedPassphrase {
    fn unlock(&self) -> anyhow::Result<String> {
        Ok(self.0.to_string())
    }
    fn create(&self) -> anyhow::Result<String> {
        Ok(self.0.to_string())
    }
}
