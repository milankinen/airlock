//! Plaintext file vault storage.
//!
//! Used for `settings.vault.storage = "file"`. Keeps the vault in a file without
//! encryption.

use std::path::PathBuf;

use anyhow::{Context, bail};

use super::{Envelope, Storage, VaultData, atomic_write, read_vault_file};

/// Storage backend that keeps the vault as plaintext JSON in one file.
///
/// The file has the tagged [`Envelope`] format. Thus, after a change to
/// `settings.vault.storage = "encrypted-file"`, the encrypted backend refuses to
/// read this file as encrypted, and the reverse. It does not silently
/// start an empty vault.
pub struct FileStorage {
    path: PathBuf,
}

impl FileStorage {
    /// Make a backend for the vault file at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Storage for FileStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        let Some(raw) = read_vault_file(&self.path)? else {
            return Ok(None);
        };
        match serde_json::from_str::<Envelope>(&raw)
            .with_context(|| format!("parse vault file {}", self.path.display()))?
        {
            Envelope::File(data) => Ok(Some(
                serde_json::to_string(&data).context("re-serialize vault data")?,
            )),
            Envelope::EncryptedFile(_) => bail!(
                "{} is an encrypted vault, but vault.storage = \"file\" in settings. \
                 Set vault.storage = \"encrypted-file\" (or delete the file to start fresh).",
                self.path.display()
            ),
        }
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        let parsed: VaultData = serde_json::from_str(data).context("parse vault data")?;
        let envelope = Envelope::File(parsed);
        let json = serde_json::to_string_pretty(&envelope).context("serialize vault envelope")?;
        atomic_write(&self.path, json.as_bytes())
    }

    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(Some(self.path.with_extension("lock")))
    }
}
