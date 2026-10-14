//! Plaintext file vault storage.
//!
//! Used for `settings.vault.storage = "file"`. Keeps the vault in a file without
//! encryption.

use anyhow::{Context, bail};

use super::{Envelope, Storage, VaultData, atomic_write, read_vault_file, vault_lock_file};
use crate::messages;
use crate::util::VerifiedPath;

/// Storage backend that keeps the vault as plaintext JSON in one file.
///
/// The file has the tagged [`Envelope`] format. Thus, after a change to
/// `settings.vault.storage = "encrypted-file"`, the encrypted backend refuses to
/// read this file as encrypted, and the reverse. It does not silently
/// start an empty vault.
pub struct FileStorage {
    path: VerifiedPath,
}

impl FileStorage {
    /// Make a backend for the vault file at `path`.
    pub fn new(path: VerifiedPath) -> Self {
        Self { path }
    }
}

impl Storage for FileStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        let Some(raw) = read_vault_file(&self.path)? else {
            return Ok(None);
        };
        match serde_json::from_str::<Envelope>(&raw)
            .with_context(|| messages::vault::parse_vault_file(&self.path))?
        {
            Envelope::File(data) => Ok(Some(
                serde_json::to_string(&data).context(messages::vault::RESERIALIZE_VAULT_DATA)?,
            )),
            Envelope::EncryptedFile(_) => bail!(messages::vault::encrypted_vault_but_file_storage(
                &self.path
            )),
        }
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        let parsed: VaultData =
            serde_json::from_str(data).context(messages::vault::PARSE_VAULT_DATA)?;
        let envelope = Envelope::File(parsed);
        let json = serde_json::to_string_pretty(&envelope)
            .context(messages::vault::SERIALIZE_VAULT_ENVELOPE)?;
        atomic_write(&self.path, json.as_bytes())
    }

    fn lock_path(&self) -> anyhow::Result<Option<VerifiedPath>> {
        vault_lock_file(&self.path).map(Some)
    }
}
