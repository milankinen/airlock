//! Encrypted file vault storage.
//!
//! Used for `settings.vault.storage = "encrypted-file"`. Keeps the vault in a file
//! that a passphrase encrypts. Gets the passphrase from an environment
//! variable, or asks the user for it.

use std::path::PathBuf;

use anyhow::{Context, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use parking_lot::Mutex;
use rand::TryRng;
use rand::rngs::SysRng;

use super::{
    ARGON2_KEY_BYTES, ARGON2_M_KIB, ARGON2_P, ARGON2_T, EncryptedBlob, Envelope, KdfParams,
    NONCE_BYTES, SALT_BYTES, Storage, atomic_write, decode_b64_array, read_vault_file,
};
use crate::cli::prompt::fields::{Field, Fields, Invalid};

/// Env var that gives the encrypted-vault passphrase without a prompt
/// (CI, scripts, and headless shells without a TTY).
const PASSPHRASE_ENV: &str = "AIRLOCK_VAULT_PASSPHRASE";

/// Source of the passphrase for an [`EncryptedFileStorage`]. Tests use it
/// to inject a fixed value without a TTY or env var.
pub trait PassphraseSource: Send + Sync + 'static {
    /// Ask for the passphrase of an existing vault.
    fn unlock(&self) -> anyhow::Result<String>;
    /// Ask for a new passphrase for a new vault.
    fn create(&self) -> anyhow::Result<String>;
}

/// Storage backend that keeps the vault as encrypted JSON in one file. The
/// key comes from the passphrase and a per-vault salt. The user gives the
/// passphrase one time per process.
pub struct EncryptedFileStorage {
    path: PathBuf,
    passphrase: Box<dyn PassphraseSource>,
    /// Cached key. Derived on the first unlock or create, and used again
    /// after that, so the user gets only one prompt per process.
    key: Mutex<Option<[u8; ARGON2_KEY_BYTES]>>,
    /// Cached salt of the vault. Used again, so the derived key stays the
    /// same across reads in one process. `None` until the first successful
    /// load, or until a new vault is created.
    salt: Mutex<Option<[u8; SALT_BYTES]>>,
}

impl EncryptedFileStorage {
    /// Make a backend for the encrypted vault file.
    /// Args:
    ///  - `path`: Path of the vault file
    ///  - `passphrase`: Source of the passphrase
    pub fn new(path: PathBuf, passphrase: Box<dyn PassphraseSource>) -> Self {
        Self {
            path,
            passphrase,
            key: Mutex::new(None),
            salt: Mutex::new(None),
        }
    }

    /// Derive the encryption key from a passphrase with Argon2id.
    /// Args:
    ///  - `passphrase`: User passphrase
    ///  - `salt`: Per-vault salt
    ///  - `m_kib`, `t`, `p`: Argon2 memory cost (KiB), time cost and
    ///    parallelism
    fn derive_key(
        passphrase: &str,
        salt: &[u8],
        m_kib: u32,
        t: u32,
        p: u32,
    ) -> anyhow::Result<[u8; ARGON2_KEY_BYTES]> {
        let params = Params::new(m_kib, t, p, Some(ARGON2_KEY_BYTES))
            .map_err(|e| anyhow!("invalid argon2 params: {e}"))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
        let mut out = [0u8; ARGON2_KEY_BYTES];
        argon2
            .hash_password_into(passphrase.as_bytes(), salt, &mut out)
            .map_err(|e| anyhow!("argon2id kdf failed: {e}"))?;
        Ok(out)
    }
}

/// Upper limits of the Argon2 parameters that a vault file can have. They
/// prevent a hostile or corrupt file from causing a very large memory
/// allocation (DoS). The limits are much higher than the values that this
/// version writes (19 MiB / t=2 / p=1), but still sane.
const MAX_ARGON2_M_KIB: u32 = 1 << 20; // 1 GiB
const MAX_ARGON2_T: u32 = 16;
const MAX_ARGON2_P: u32 = 16;

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
                "{} is a plaintext vault, but vault.storage = \"encrypted-file\" in settings. \
                 Set vault.storage = \"file\" (or delete the file to re-create encrypted).",
                self.path.display()
            ),
        };

        if blob.kdf.algo != "argon2id" {
            bail!("unsupported vault KDF algo: {}", blob.kdf.algo);
        }
        // Derive the key with the m/t/p values in the file, not with the
        // constants. A later release or another tool can write other values.
        // The limits stop a hostile file from causing a very large allocation.
        if blob.kdf.m > MAX_ARGON2_M_KIB || blob.kdf.t > MAX_ARGON2_T || blob.kdf.p > MAX_ARGON2_P {
            bail!(
                "vault KDF parameters out of bounds (m={} t={} p={})",
                blob.kdf.m,
                blob.kdf.t,
                blob.kdf.p
            );
        }
        let salt = decode_b64_array::<SALT_BYTES>(&blob.kdf.salt, "salt")?;
        let nonce = decode_b64_array::<NONCE_BYTES>(&blob.nonce, "nonce")?;
        let ciphertext = STANDARD_NO_PAD
            .decode(&blob.ciphertext)
            .context("decode vault ciphertext")?;

        // Use the cached key again if this process already unlocked the
        // vault and the salt in the file did not change. Thus a read during
        // a write (lock → reload → merge → store) does not ask again.
        let cached_key = {
            let key = self.key.lock();
            let cached_salt = self.salt.lock();
            match (*key, *cached_salt) {
                (Some(k), Some(s)) if s == salt => Some(k),
                _ => None,
            }
        };
        let key = if let Some(k) = cached_key {
            k
        } else {
            let passphrase = self.passphrase.unlock()?;
            Self::derive_key(&passphrase, &salt, blob.kdf.m, blob.kdf.t, blob.kdf.p)?
        };
        let cipher = ChaCha20Poly1305::new(<&Key>::from(&key));
        let plaintext = cipher
            .decrypt(<&Nonce>::from(&nonce), ciphertext.as_ref())
            .map_err(|_| anyhow!("decrypt vault: wrong passphrase or corrupt data"))?;

        *self.key.lock() = Some(key);
        *self.salt.lock() = Some(salt);

        Ok(Some(
            String::from_utf8(plaintext).context("decrypted vault is not valid UTF-8")?,
        ))
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        // Use the same salt (and thus the same key) for all writes in one
        // process. A new vault has no cached salt. Then make a salt and ask
        // for a new passphrase.
        let (salt, key) = {
            let mut salt_slot = self.salt.lock();
            let mut key_slot = self.key.lock();
            if let (Some(s), Some(k)) = (*salt_slot, *key_slot) {
                (s, k)
            } else {
                let mut salt = [0u8; SALT_BYTES];
                SysRng
                    .try_fill_bytes(&mut salt)
                    .context("generate vault salt")?;
                let passphrase = self.passphrase.create()?;
                let key = Self::derive_key(&passphrase, &salt, ARGON2_M_KIB, ARGON2_T, ARGON2_P)?;
                *salt_slot = Some(salt);
                *key_slot = Some(key);
                (salt, key)
            }
        };

        // Each write uses a new random nonce. ChaCha20-Poly1305 is not safe
        // when one key and nonce encrypt two messages.
        let mut nonce = [0u8; NONCE_BYTES];
        SysRng
            .try_fill_bytes(&mut nonce)
            .context("generate vault nonce")?;
        let cipher = ChaCha20Poly1305::new(<&Key>::from(&key));
        let ciphertext = cipher
            .encrypt(<&Nonce>::from(&nonce), data.as_bytes())
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

    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(Some(self.path.with_extension("lock")))
    }
}

/// Passphrase source of the CLI. Uses `AIRLOCK_VAULT_PASSPHRASE` first
/// (for CI and non-interactive runs). If it is not set, shows a terminal
/// prompt that does not echo the input. After input, the prompt line is
/// erased.
pub struct InteractivePassphrase;

/// Make the passphrase source of the CLI, see [`InteractivePassphrase`].
pub(super) fn interactive_passphrase() -> Box<dyn PassphraseSource> {
    Box::new(InteractivePassphrase)
}

impl PassphraseSource for InteractivePassphrase {
    fn unlock(&self) -> anyhow::Result<String> {
        if let Ok(p) = std::env::var(PASSPHRASE_ENV) {
            return Ok(p);
        }
        prompt_once("vault passphrase")
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

/// Ask for the passphrase of an existing vault. Erases the line after
/// input.
/// Args:
///  - `label`: Label of the input row
fn prompt_once(label: &str) -> anyhow::Result<String> {
    if !crate::cli::is_interactive() {
        bail!(
            "no TTY available to prompt for the vault passphrase — set {PASSPHRASE_ENV} \
             or run from an interactive terminal"
        );
    }
    let form = Fields {
        title: None,
        rows: &[Field {
            label,
            secret: true,
        }],
        keys: "enter unlock · esc cancel",
    };
    let texts = form
        .ask(|texts| required(texts, 0))
        .context("read vault passphrase")?;
    let Some(mut texts) = texts else {
        bail!("vault passphrase prompt cancelled");
    };
    Ok(texts.swap_remove(0))
}

/// Ask two times for the passphrase of a new vault. Erases the lines after
/// input.
fn prompt_create() -> anyhow::Result<String> {
    if !crate::cli::is_interactive() {
        bail!(
            "no TTY available to set a new vault passphrase — set {PASSPHRASE_ENV} \
             or run from an interactive terminal"
        );
    }
    let form = Fields {
        title: Some("Set a vault passphrase"),
        rows: &[
            Field {
                label: "new passphrase",
                secret: true,
            },
            Field {
                label: "confirm",
                secret: true,
            },
        ],
        keys: "enter next · esc cancel",
    };
    let check = |texts: &[String]| {
        required(texts, 0)?;
        if texts[0] == texts[1] {
            Ok(())
        } else {
            Err(Invalid {
                field: 1,
                message: "passphrases do not match".to_string(),
            })
        }
    };
    let texts = form.ask(check).context("read vault passphrase")?;
    let Some(mut texts) = texts else {
        bail!("vault passphrase prompt cancelled");
    };
    Ok(texts.swap_remove(0))
}

/// Validate a passphrase form: the passphrase in row `field` must not be
/// empty.
fn required(texts: &[String], field: usize) -> Result<(), Invalid> {
    if texts[field].is_empty() {
        return Err(Invalid {
            field,
            message: "vault passphrase must not be empty".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for the key derivation parameters in the encrypted vault file.

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    use super::{
        ARGON2_M_KIB, ARGON2_P, ARGON2_T, EncryptedBlob, EncryptedFileStorage, Envelope, KdfParams,
        MAX_ARGON2_M_KIB, NONCE_BYTES, SALT_BYTES, Storage, atomic_write,
    };
    use crate::test_cfg::temp_dir;
    use crate::test_cfg::vault::FixedPassphrase;

    /// Write `envelope` to a vault file and load it with the passphrase
    /// `pass`.
    fn load_envelope(envelope: &Envelope, pass: &'static str) -> anyhow::Result<Option<String>> {
        let tmp = temp_dir();
        let path = tmp.path().join("vault.enc.json");
        atomic_write(
            &path,
            serde_json::to_string_pretty(envelope).unwrap().as_bytes(),
        )
        .unwrap();
        EncryptedFileStorage::new(path, Box::new(FixedPassphrase(pass))).load()
    }

    /// Test that the vault decrypts with the KDF parameters in the file, not
    /// with the current constants, so that a vault from another version
    /// still opens.
    ///   1. Encrypt a blob with a key made with a different `t` value
    ///   2. Write the blob and its KDF parameters to a vault file
    ///   3. Load the file and check the plaintext
    #[test]
    fn vault_file_decrypts_with_kdf_params_stored_in_it() {
        let salt = [7u8; SALT_BYTES];
        let t = ARGON2_T + 1;
        let key =
            EncryptedFileStorage::derive_key("hunter2", &salt, ARGON2_M_KIB, t, ARGON2_P).unwrap();
        let mut nonce = [0u8; NONCE_BYTES];
        nonce[0] = 1;
        let plaintext = r#"{"secrets":{},"registries":{}}"#;
        let ct = ChaCha20Poly1305::new(<&Key>::from(&key))
            .encrypt(<&Nonce>::from(&nonce), plaintext.as_bytes())
            .unwrap();
        let envelope = Envelope::EncryptedFile(EncryptedBlob {
            kdf: KdfParams {
                algo: "argon2id".to_string(),
                salt: STANDARD_NO_PAD.encode(salt),
                m: ARGON2_M_KIB,
                t,
                p: ARGON2_P,
            },
            nonce: STANDARD_NO_PAD.encode(nonce),
            ciphertext: STANDARD_NO_PAD.encode(&ct),
        });

        assert_eq!(
            load_envelope(&envelope, "hunter2").unwrap().as_deref(),
            Some(plaintext)
        );
    }

    /// Test that the vault refuses KDF parameters above the limits, so that a
    /// hostile file cannot cause a very large memory allocation.
    ///   1. Write a vault file with a memory cost above the limit
    ///   2. Load the file and check the "out of bounds" error
    #[test]
    fn vault_file_with_out_of_bounds_kdf_params_is_refused() {
        let envelope = Envelope::EncryptedFile(EncryptedBlob {
            kdf: KdfParams {
                algo: "argon2id".to_string(),
                salt: STANDARD_NO_PAD.encode([0u8; SALT_BYTES]),
                m: MAX_ARGON2_M_KIB + 1,
                t: ARGON2_T,
                p: ARGON2_P,
            },
            nonce: STANDARD_NO_PAD.encode([0u8; NONCE_BYTES]),
            ciphertext: STANDARD_NO_PAD.encode([0u8; 32]),
        });

        let err = load_envelope(&envelope, "pw").unwrap_err();
        assert!(err.to_string().contains("out of bounds"), "{err:#}");
    }
}
