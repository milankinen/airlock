//! Secret storage.
//!
//! Stores the secrets that the user adds with `airlock secrets`. Projects
//! refer to these secrets by name in their config, and airlock puts in the
//! real values. The vault also stores credentials for image registries.
//!
//! The user selects where the vault keeps its data in `settings.vault`: the
//! system keychain, an encrypted file, a plaintext file, or nothing. The rest
//! of the program uses the vault in the same way for all storage types.

mod disabled;
mod encrypted;
mod file;
mod keyring;
pub(crate) mod ui;

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
pub(crate) use disabled::DisabledStorage;
use encrypted::EncryptedFileStorage;
#[cfg(test)]
pub(crate) use encrypted::PassphraseSource;
use file::FileStorage;
use keyring::KeyringStorage;
use parking_lot::{Mutex, MutexGuard};
use rand::TryRng as _;
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

// Argon2id parameters from the OWASP 2023 "second recommendation": 19 MiB
// memory, t=2, p=1. These values are safe and keep an interactive unlock
// on a laptop fast (approximately 100-300 ms).
/// Argon2id memory cost in KiB.
pub(crate) const ARGON2_M_KIB: u32 = 19_456;
/// Argon2id time cost (iterations).
pub(crate) const ARGON2_T: u32 = 2;
/// Argon2id parallelism.
pub(crate) const ARGON2_P: u32 = 1;
/// Length of the derived encryption key in bytes.
pub(crate) const ARGON2_KEY_BYTES: usize = 32;
/// Length of the KDF salt in bytes.
pub(crate) const SALT_BYTES: usize = 16;
/// Length of the ChaCha20-Poly1305 nonce in bytes.
pub(crate) const NONCE_BYTES: usize = 12;

/// One user-managed secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SecretEntry {
    value: String,
    saved_at: SystemTime,
}

/// Image-registry credentials for one host.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RegistryEntry {
    username: String,
    password: String,
    saved_at: SystemTime,
}

/// Metadata of one secret, returned by [`Vault::list_secrets`]. Does not
/// contain the secret value.
#[derive(Clone, Debug)]
pub struct SecretMeta {
    /// Secret name.
    pub name: String,
    /// Time of the last write of the secret.
    pub saved_at: SystemTime,
    /// Masked preview of the value, see [`secret_preview`]. Use it only to
    /// tell apart entries with similar names.
    pub preview: String,
}

/// Make a masked preview of a secret value. The preview is safe to show
/// next to the secret name.
/// Args:
///  - `value`: Secret value
///
/// Returns:
///   `****` followed by the last 4 chars when the value has 16 or more
///   chars, the last 2 chars when it has 8 or more, and no chars otherwise.
pub fn secret_preview(value: &str) -> String {
    // The fixed `****` prefix hides the total length. Below 8 chars, even
    // two shown chars are a large part of the secret's entropy.
    let len = value.chars().count();
    let tail = if len >= 16 {
        4
    } else if len >= 8 {
        2
    } else {
        0
    };
    let mut out = String::from("****");
    if tail > 0 {
        out.extend(value.chars().skip(len - tail));
    }
    out
}

/// Image-registry credentials for one host. Callers can make them without
/// access to the internal storage types.
#[derive(Clone, Debug)]
pub struct RegistryCreds {
    /// Registry user name.
    pub username: String,
    /// Registry password or token.
    pub password: String,
}

/// Top-level field of the agent credentials of an earlier unreleased
/// version (`airlock agents`). The next write drops it, because it holds
/// real tokens and keys that no command can show or remove.
const RETIRED_AGENTS_SECTION: &str = "agents";

/// Contents of the vault. All items live in this one blob, which the
/// [`Storage`] backend stores as JSON.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct VaultData {
    #[serde(default)]
    secrets: BTreeMap<String, SecretEntry>,
    #[serde(default)]
    registries: BTreeMap<String, RegistryEntry>,
    /// Key of the token store of the network services (in `~/.airlock/db/`):
    /// 32 random bytes, base64. Created one time. It is not a user secret,
    /// so `airlock secrets` does not list it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_store_key: Option<String>,
    /// Top-level fields that this version does not know. A write keeps
    /// them, so a later format change survives a write by this version.
    /// The exception is [`RETIRED_AGENTS_SECTION`], which a write drops.
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// Storage backend of the [`Vault`]. Matches `settings.vault.storage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VaultStorageType {
    /// No storage. Reads return an empty vault and writes are dropped.
    /// `airlock secrets` refuses to run.
    Disabled,
    /// Plaintext JSON at `~/.airlock/vault.default.json` (mode 0600).
    File,
    /// AEAD-encrypted JSON at `~/.airlock/vault.default.enc.json`. The
    /// passphrase comes from `AIRLOCK_VAULT_PASSPHRASE` or an interactive
    /// prompt.
    EncryptedFile,
    /// OS keychain / Secret Service.
    #[default]
    Keyring,
}

impl smart_config::de::WellKnown for VaultStorageType {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Process-global vault handle. Clones share one state, so a clone is
/// cheap. Use one vault per process.
///
/// The vault opens its storage lazily: construction does not touch the
/// storage, and the first getter or setter call opens it. For
/// `encrypted-file`, this call asks for the passphrase. For `keyring` on
/// Linux, this call can cause a Secret Service unlock.
///
/// Each write reads, changes and writes the whole blob under a
/// cross-process lock file (see [`Storage::lock_path`]). Thus a writer
/// never drops what another process wrote.
#[derive(Clone)]
pub struct Vault {
    inner: Arc<VaultInner>,
}

struct VaultInner {
    storage: Box<dyn Storage>,
    /// Cached vault contents. `None` until the vault opens. Reads clone the
    /// necessary fields, so the lock is never held across foreign code.
    data: Mutex<Option<VaultData>>,
    /// Host environment snapshot for `${NAME}` substitution. It is fixed,
    /// so tests can inject a known env, and substitution gives the same
    /// result when other code changes `std::env` during the run.
    env: HashMap<String, String>,
    /// Backend of this vault. The CLI uses it to warn the user when
    /// `secret add` writes into a plaintext file.
    storage_type: VaultStorageType,
}

impl Default for Vault {
    fn default() -> Self {
        Self::for_storage_type(VaultStorageType::default())
    }
}

impl Vault {
    /// Make a vault for the given storage backend. Takes the host
    /// environment snapshot now. Does not open the storage.
    pub fn for_storage_type(storage_type: VaultStorageType) -> Self {
        Self::new_with(
            boxed_storage(storage_type),
            std::env::vars().collect(),
            storage_type,
        )
    }

    /// Make a vault with a custom storage backend and a fixed env map.
    /// For tests. The CLI uses [`Vault::for_storage_type`].
    /// Args:
    ///  - `storage`: Storage backend
    ///  - `env`: Host environment for `${NAME}` substitution
    ///  - `storage_type`: Backend type that [`Vault::storage_type`] returns
    pub fn new_with(
        storage: Box<dyn Storage>,
        env: HashMap<String, String>,
        storage_type: VaultStorageType,
    ) -> Self {
        Self {
            inner: Arc::new(VaultInner {
                storage,
                data: Mutex::new(None),
                env,
                storage_type,
            }),
        }
    }

    /// Backend of this vault. Subcommands use it to change their behavior
    /// (for example, `secret add` gives a warning for `File`).
    pub fn storage_type(&self) -> VaultStorageType {
        self.inner.storage_type
    }

    fn open(&self) -> anyhow::Result<OpenedVault<'_>> {
        let mut guard = self.inner.data.lock();
        if guard.is_none() {
            *guard = Some(self.load()?);
        }
        Ok(OpenedVault(guard))
    }

    /// Read the current blob from storage. "No vault yet" (no file, no
    /// keyring entry) is not an error. It gives an empty vault.
    fn load(&self) -> anyhow::Result<VaultData> {
        match self.inner.storage.load()? {
            Some(json) => serde_json::from_str::<VaultData>(&json)
                .context("parse airlock vault blob — storage may be corrupt"),
            None => Ok(VaultData::default()),
        }
    }

    /// Take the cross-process vault lock of the backend, if it has one.
    /// The lock stays until the returned handle drops.
    fn lock_storage(&self) -> anyhow::Result<Option<File>> {
        self.inner
            .storage
            .lock_path()?
            .map(|path| acquire_file_lock(&path))
            .transpose()
    }

    fn flush(&self, data: &VaultData) -> anyhow::Result<()> {
        let json = serde_json::to_string(data).context("serialize airlock vault")?;
        self.inner.storage.store(&json)
    }

    /// Change the vault contents under the cross-process lock.
    /// Args:
    ///  - `f`: Function that changes the current stored contents
    ///
    /// Returns:
    ///   Result of `f`. When `f` fails, nothing is written.
    fn mutate<R>(&self, f: impl FnOnce(&mut VaultData) -> anyhow::Result<R>) -> anyhow::Result<R> {
        // Read the latest state under the lock. The cached snapshot can be
        // old, and a write from it can erase secrets that a concurrent
        // `airlock secrets add` wrote. While this call holds the vault lock,
        // it takes only the cache lock, for a short time at the end.
        let _lock = self.lock_storage()?;
        let mut data = self.load()?;
        data.unknown.remove(RETIRED_AGENTS_SECTION);
        let result = f(&mut data)?;
        self.flush(&data)?;
        *self.inner.data.lock() = Some(data);
        Ok(result)
    }

    /// Find a user secret by name. Opens the vault on first use.
    /// Returns:
    ///   Secret value, or `None` if the vault has no secret with this name.
    pub fn get_secret(&self, name: &str) -> anyhow::Result<Option<String>> {
        let opened = self.open()?;
        Ok(opened.data().secrets.get(name).map(|e| e.value.clone()))
    }

    /// Store or overwrite a user secret.
    /// Args:
    ///  - `name`: Secret name. Must be a valid env-var name, see
    ///    [`validate_secret_name`]
    ///  - `value`: Secret value. Must not be empty
    pub fn set_secret(&self, name: &str, value: &str) -> anyhow::Result<()> {
        validate_secret_name(name)?;
        if value.is_empty() {
            bail!("secret value must not be empty");
        }
        self.mutate(|data| {
            data.secrets.insert(
                name.to_string(),
                SecretEntry {
                    value: value.to_string(),
                    saved_at: SystemTime::now(),
                },
            );
            Ok(())
        })
    }

    /// Remove a user secret.
    /// Returns:
    ///   `false` if the vault had no secret with this name. Thus the CLI
    ///   can report "nothing to do" separately from storage errors.
    pub fn remove_secret(&self, name: &str) -> anyhow::Result<bool> {
        self.mutate(|data| Ok(data.secrets.remove(name).is_some()))
    }

    /// List the secrets (names, timestamps and masked previews, but not
    /// the values).
    pub fn list_secrets(&self) -> anyhow::Result<Vec<SecretMeta>> {
        let opened = self.open()?;
        Ok(opened
            .data()
            .secrets
            .iter()
            .map(|(name, entry)| SecretMeta {
                name: name.clone(),
                saved_at: entry.saved_at,
                preview: secret_preview(&entry.value),
            })
            .collect())
    }

    /// Find the registry credentials for `host`.
    pub fn get_registry(&self, host: &str) -> anyhow::Result<Option<RegistryCreds>> {
        let opened = self.open()?;
        Ok(opened.data().registries.get(host).map(|e| RegistryCreds {
            username: e.username.clone(),
            password: e.password.clone(),
        }))
    }

    /// Store or overwrite registry credentials for `host`.
    pub fn set_registry(&self, host: &str, creds: &RegistryCreds) -> anyhow::Result<()> {
        if host.is_empty() {
            bail!("registry host must not be empty");
        }
        self.mutate(|data| {
            data.registries.insert(
                host.to_string(),
                RegistryEntry {
                    username: creds.username.clone(),
                    password: creds.password.clone(),
                    saved_at: SystemTime::now(),
                },
            );
            Ok(())
        })
    }

    /// Get the key of the token store of the network services. Creates the
    /// key on first use.
    pub fn service_store_key(&self) -> anyhow::Result<[u8; 32]> {
        // The key is created under the vault lock, so concurrent first uses
        // get the same key.
        let stored = self.open()?.data().service_store_key.clone();
        if let Some(key) = stored {
            return decode_b64_array(&key, "service store key");
        }
        self.mutate(|data| {
            if let Some(key) = &data.service_store_key {
                return decode_b64_array(key, "service store key");
            }
            let mut key = [0u8; 32];
            rand::rngs::SysRng
                .try_fill_bytes(&mut key)
                .context("generate the service store key")?;
            data.service_store_key = Some(STANDARD_NO_PAD.encode(key));
            Ok(key)
        })
    }

    /// Expand the `${NAME}` tokens in a template.
    /// Args:
    ///  - `template`: Text with `${NAME}` tokens
    ///
    /// Returns:
    ///   Expanded text, or an error if a name has no value. A name gets
    ///   its value from the host environment first, then from the vault
    ///   secrets.
    pub fn subst(&self, template: &str) -> anyhow::Result<String> {
        // Names that the host env defines (for example `${PATH}`) do not
        // open the vault. Only the other names open it.
        subst::substitute(template, self).map_err(|e| match self.open() {
            // The variable is missing because the vault did not open. Tell
            // why, not only that it is missing.
            Err(open) => open.context(e.to_string()),
            Ok(_) => anyhow!("{e}"),
        })
    }
}

/// Lock guard of an open vault. The data is always `Some`.
struct OpenedVault<'a>(MutexGuard<'a, Option<VaultData>>);

impl<'a> subst::VariableMap<'a> for Vault {
    type Value = String;
    fn get(&'a self, key: &str) -> Option<Self::Value> {
        if let Some(value) = self.inner.env.get(key) {
            return Some(value.clone());
        }
        self.get_secret(key).ok().flatten()
    }
}

impl OpenedVault<'_> {
    fn data(&self) -> &VaultData {
        self.0.as_ref().expect("opened vault has data")
    }
}

/// Make sure that a user-secret name is a POSIX env-var identifier
/// (`[A-Z_][A-Z0-9_]*`). Other names cannot be used in `${NAME}`.
pub fn validate_secret_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("secret name must not be empty");
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty");
    if !(first.is_ascii_uppercase() || first == '_') {
        bail!("secret name must start with A-Z or '_', got '{first}' in \"{name}\"");
    }
    for c in chars {
        if !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
            bail!("secret name must be [A-Z_][A-Z0-9_]*, got '{c}' in \"{name}\"");
        }
    }
    Ok(())
}

// ── Storage trait + dispatcher ─────────────────────────────────────────────

/// Storage backend for the vault JSON blob. The vault gives a plain
/// [`VaultData`] JSON string to the backend and gets the same string back.
/// The backend does the on-disk envelope and encryption, if any.
pub trait Storage: Send + Sync + 'static {
    /// Read the stored blob.
    /// Returns:
    ///   Blob JSON, or `None` if nothing is stored yet.
    fn load(&self) -> anyhow::Result<Option<String>>;
    /// Write the blob JSON, and replace the old blob.
    fn store(&self, data: &str) -> anyhow::Result<()>;

    /// Path of the lock file that serializes writes across processes.
    /// Returns:
    ///   `None` for backends that do not need a lock (disabled, in-memory
    ///   test doubles). An error when the backend needs a lock but cannot
    ///   name its path, so a write never runs without the lock.
    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }
}

/// Make the storage backend for the given type.
fn boxed_storage(storage_type: VaultStorageType) -> Box<dyn Storage> {
    match storage_type {
        VaultStorageType::Disabled => Box::new(DisabledStorage),
        VaultStorageType::File => Box::new(FileStorage::new(
            Settings::dir()
                .unwrap_or(PathBuf::from("."))
                .join("vault.default.json"),
        )),
        VaultStorageType::EncryptedFile => Box::new(EncryptedFileStorage::new(
            Settings::dir()
                .unwrap_or(PathBuf::from("."))
                .join("vault.default.enc.json"),
            encrypted::interactive_passphrase(),
        )),
        VaultStorageType::Keyring => Box::new(KeyringStorage),
    }
}

// ── Shared on-disk envelope ────────────────────────────────────────────────
//
// It is here (not in `encrypted.rs`), so `file.rs` can use it without an
// import from a sibling module.

/// Tagged on-disk format of the two file backends:
///
/// ```json
/// { "type": "file",           "data": { ...VaultData... } }
/// { "type": "encrypted-file", "data": { "kdf": {...}, "nonce": "...", "ciphertext": "..." } }
/// ```
///
/// After a change of `settings.vault`, a backend refuses to read the file
/// of the other backend. It does not silently start an empty vault, and it
/// rejects the file before a write.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub(crate) enum Envelope {
    /// Plaintext vault contents.
    File(VaultData),
    /// Encrypted vault contents.
    EncryptedFile(EncryptedBlob),
}

/// Encrypted vault contents and the data necessary to decrypt them.
#[derive(Serialize, Deserialize)]
pub(crate) struct EncryptedBlob {
    /// Key derivation parameters.
    pub(crate) kdf: KdfParams,
    /// 12-byte ChaCha20-Poly1305 nonce, base64 (unpadded).
    pub(crate) nonce: String,
    /// AEAD ciphertext + 16-byte tag, base64 (unpadded).
    pub(crate) ciphertext: String,
}

/// Parameters of the key derivation from the passphrase.
#[derive(Serialize, Deserialize)]
pub(crate) struct KdfParams {
    /// KDF algorithm name.
    pub(crate) algo: String,
    /// 16-byte salt, base64 (unpadded).
    pub(crate) salt: String,
    /// Memory cost (KiB).
    pub(crate) m: u32,
    /// Time cost (iterations).
    pub(crate) t: u32,
    /// Parallelism.
    pub(crate) p: u32,
}

// ── File I/O helpers ───────────────────────────────────────────────────────

/// Read a vault file.
/// Returns:
///   File contents, or `None` if the file does not exist.
pub(crate) fn read_vault_file(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("read vault file {}: {e}", path.display())),
    }
}

/// Take an exclusive advisory lock on `path`. The lock stays until the
/// returned handle drops. Serializes vault writes across processes.
fn acquire_file_lock(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create vault directory {}", parent.display()))?;
    }
    // `O_NOFOLLOW`: refuse a symlink at the lock path. Do not follow it to
    // create or lock a different file.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open vault lock {}", path.display()))?;
    // The lock blocks (writes are short), so concurrent writers wait in a
    // queue and do not fail.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(anyhow!(
            "lock vault {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

/// Write `bytes` to `path` atomically, with mode 0600. Creates the parent
/// directory if it does not exist.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    // Write to a sibling temp file and rename it, so a crash during the
    // write cannot leave a truncated vault.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create vault directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("vault path has no file name: {}", path.display()))?;
    // The temp name is unique per process. Thus two concurrent writers
    // cannot rename a half-written temp file of the other process.
    let unique = std::process::id();
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!("{}.{unique}.tmp", file_name.to_string_lossy()));

    let mut f: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create vault tempfile {}", tmp.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("write vault tempfile {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("fsync vault tempfile {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename vault tempfile to {}", path.display()))?;
    Ok(())
}

/// Decode an unpadded base64 string into a fixed-size byte array.
/// Args:
///  - `s`: Base64 text
///  - `label`: Name of the value for error messages
///
/// Returns:
///   Decoded bytes, or an error if the text is not base64 or the length
///   is not `N`.
pub(crate) fn decode_b64_array<const N: usize>(s: &str, label: &str) -> anyhow::Result<[u8; N]> {
    let bytes = STANDARD_NO_PAD
        .decode(s)
        .with_context(|| format!("decode vault {label}"))?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow!("vault {label} has wrong length: expected {N} bytes"))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
