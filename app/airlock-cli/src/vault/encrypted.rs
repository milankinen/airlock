//! AEAD-encrypted JSON backend. Envelope format lives in the parent
//! module (so `file.rs` can peek at the `type` tag and refuse to open
//! the wrong kind of vault); this file holds the crypto and the
//! passphrase UX.
//!
//! Argon2id (OWASP 2023 second recommendation: 19 MiB memory, t=2,
//! p=1) derives a 32-byte key from the user's passphrase and a
//! per-vault salt. ChaCha20-Poly1305 AEAD encrypts the JSON blob under
//! a fresh 12-byte nonce on each write. Salt + key are cached in
//! memory after the first successful unlock so the user is only
//! prompted once per process.

use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use parking_lot::Mutex;
use rand::TryRngCore;
use rand::rngs::OsRng;

use super::{
    ARGON2_KEY_BYTES, ARGON2_M_KIB, ARGON2_P, ARGON2_T, EncryptedBlob, Envelope, KdfParams,
    NONCE_BYTES, SALT_BYTES, Storage, atomic_write, decode_b64_array, read_vault_file,
};

/// Env var used to supply the encrypted-vault passphrase non-interactively
/// (CI, scripts, and headless shells without a TTY).
const PASSPHRASE_ENV: &str = "AIRLOCK_VAULT_PASSPHRASE";

/// Supplies the passphrase for an `EncryptedFileStorage`. Abstracted so
/// tests can inject a fixed value without a TTY or env-var round-trip.
pub trait PassphraseSource: Send + Sync + 'static {
    /// Ask for the passphrase of an existing vault.
    fn unlock(&self) -> anyhow::Result<String>;
    /// Ask for a new passphrase when the vault is being created.
    fn create(&self) -> anyhow::Result<String>;
}

pub struct EncryptedFileStorage {
    path: PathBuf,
    passphrase: Box<dyn PassphraseSource>,
    /// Cached key, derived on first unlock/create and reused on
    /// subsequent operations so the user only prompts once per process.
    key: Mutex<Option<[u8; ARGON2_KEY_BYTES]>>,
    /// Cached salt for an existing vault — reused so the derived key
    /// stays stable across reads in one process. `None` until the
    /// first successful load, or until a fresh vault is created.
    salt: Mutex<Option<[u8; SALT_BYTES]>>,
}

impl EncryptedFileStorage {
    pub fn new(path: PathBuf, passphrase: Box<dyn PassphraseSource>) -> Self {
        Self {
            path,
            passphrase,
            key: Mutex::new(None),
            salt: Mutex::new(None),
        }
    }

    fn derive_key(passphrase: &str, salt: &[u8]) -> anyhow::Result<[u8; ARGON2_KEY_BYTES]> {
        let params = Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, Some(ARGON2_KEY_BYTES))
            .map_err(|e| anyhow!("invalid argon2 params: {e}"))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = [0u8; ARGON2_KEY_BYTES];
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, &mut out)
            .map_err(|e| anyhow!("argon2id kdf failed: {e}"))?;
        Ok(out)
    }
}

impl Storage for EncryptedFileStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        let Some(raw) = read_vault_file(&self.path)? else {
            return Ok(None);
        };
        let envelope: Envelope = serde_json::from_str(&raw)
            .with_context(|| format!("parse vault file {}", self.path.display()))?;
        let blob = match envelope {
            Envelope::EncryptedFile(b) => b,
            Envelope::File(_) => bail!(
                "{} is a plaintext vault, but settings.vault = \"encrypted-file\". \
                 Set settings.vault = \"file\" (or delete the file to re-create encrypted).",
                self.path.display()
            ),
        };

        if blob.kdf.algo != "argon2id" {
            bail!("unsupported vault KDF algo: {}", blob.kdf.algo);
        }
        let salt = decode_b64_array::<SALT_BYTES>(&blob.kdf.salt, "salt")?;
        let nonce = decode_b64_array::<NONCE_BYTES>(&blob.nonce, "nonce")?;
        let ciphertext = STANDARD_NO_PAD
            .decode(&blob.ciphertext)
            .context("decode vault ciphertext")?;

        let passphrase = self.passphrase.unlock()?;
        let key = Self::derive_key(&passphrase, &salt)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| anyhow!("decrypt vault: wrong passphrase or corrupt data"))?;

        *self.key.lock() = Some(key);
        *self.salt.lock() = Some(salt);

        Ok(Some(
            String::from_utf8(plaintext).context("decrypted vault is not valid UTF-8")?,
        ))
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        // Reuse the salt (and therefore the derived key) across saves
        // in the same process. On a brand-new vault there is no cached
        // salt yet — mint one and prompt for a new passphrase.
        let (salt, key) = {
            let mut salt_slot = self.salt.lock();
            let mut key_slot = self.key.lock();
            if let (Some(s), Some(k)) = (*salt_slot, *key_slot) {
                (s, k)
            } else {
                let mut salt = [0u8; SALT_BYTES];
                OsRng
                    .try_fill_bytes(&mut salt)
                    .context("generate vault salt")?;
                let passphrase = self.passphrase.create()?;
                let key = Self::derive_key(&passphrase, &salt)?;
                *salt_slot = Some(salt);
                *key_slot = Some(key);
                (salt, key)
            }
        };

        let mut nonce = [0u8; NONCE_BYTES];
        OsRng
            .try_fill_bytes(&mut nonce)
            .context("generate vault nonce")?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), data.as_bytes())
            .map_err(|e| anyhow!("encrypt vault: {e}"))?;

        let envelope = Envelope::EncryptedFile(EncryptedBlob {
            kdf: KdfParams {
                algo: "argon2id".to_string(),
                salt: STANDARD_NO_PAD.encode(salt),
                m: ARGON2_M_KIB,
                t: ARGON2_T,
                p: ARGON2_P,
            },
            nonce: STANDARD_NO_PAD.encode(nonce),
            ciphertext: STANDARD_NO_PAD.encode(&ciphertext),
        });
        let json =
            serde_json::to_string_pretty(&envelope).context("serialize encrypted envelope")?;
        atomic_write(&self.path, json.as_bytes())
    }
}

/// Production passphrase source: checks `AIRLOCK_VAULT_PASSPHRASE`
/// first (covers CI / non-interactive runs), then falls back to a
/// suppressed-echo terminal prompt. On successful input the prompt
/// line is erased so the terminal stays clean.
pub struct InteractivePassphrase;

pub(super) fn interactive_passphrase() -> Box<dyn PassphraseSource> {
    Box::new(InteractivePassphrase)
}

impl PassphraseSource for InteractivePassphrase {
    fn unlock(&self) -> anyhow::Result<String> {
        if let Ok(p) = std::env::var(PASSPHRASE_ENV) {
            return Ok(p);
        }
        prompt_once("Vault passphrase")
    }

    fn create(&self) -> anyhow::Result<String> {
        if let Ok(p) = std::env::var(PASSPHRASE_ENV) {
            if p.is_empty() {
                bail!("{PASSPHRASE_ENV} is empty");
            }
            return Ok(p);
        }
        prompt_create()
    }
}

fn prompt_once(label: &str) -> anyhow::Result<String> {
    if !crate::cli::is_interactive() {
        bail!(
            "no TTY available to prompt for the vault passphrase — set {PASSPHRASE_ENV} \
             or run from an interactive terminal"
        );
    }
    let term = console::Term::stderr();
    let pass = dialoguer::Password::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(label)
        .report(false)
        .interact_on(&term)
        .context("read vault passphrase")?;
    // Erase the prompt line so the terminal stays clean.
    let _ = term.clear_last_lines(1);
    if pass.is_empty() {
        bail!("vault passphrase must not be empty");
    }
    Ok(pass)
}

fn prompt_create() -> anyhow::Result<String> {
    if !crate::cli::is_interactive() {
        bail!(
            "no TTY available to set a new vault passphrase — set {PASSPHRASE_ENV} \
             or run from an interactive terminal"
        );
    }
    let term = console::Term::stderr();
    let pass = dialoguer::Password::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("New vault passphrase")
        .with_confirmation("Confirm passphrase", "Passphrases do not match")
        .report(false)
        .interact_on(&term)
        .context("read vault passphrase")?;
    let _ = term.clear_last_lines(2);
    if pass.is_empty() {
        bail!("vault passphrase must not be empty");
    }
    Ok(pass)
}
