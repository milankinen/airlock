//! Plaintext JSON backend. Writes a tagged envelope so a later
//! `settings.vault = "encrypted-file"` flip can refuse to reinterpret
//! a plaintext file as encrypted (and vice versa in `encrypted.rs`),
//! rather than silently zeroing a vault.

use std::path::PathBuf;

use anyhow::{Context, bail};

use super::{Envelope, Storage, VaultData, atomic_write, read_vault_file};

pub struct FileStorage {
    path: PathBuf,
}

impl FileStorage {
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
                "{} is an encrypted vault, but settings.vault = \"file\". \
                 Set settings.vault = \"encrypted-file\" (or delete the file to start fresh).",
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
}
