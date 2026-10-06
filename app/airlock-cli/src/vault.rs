//! Secret storage for airlock.
//!
//! Holds two kinds of items:
//!
//! - `secrets`: user-managed secrets (`airlock secrets add/list/remove`)
//!   exposed to projects via `${NAME}` substitution.
//! - `registries`: image-registry credentials.
//!
//! All kinds live inside a **single** `VaultData` blob. Where that blob
//! lives is chosen by `settings.vault`:
//!
//! - `keyring` (default): OS keychain / Secret Service.
//! - `encrypted-file`: `~/.airlock/vault.default.enc.json`, AEAD-encrypted with a passphrase.
//! - `file`: `~/.airlock/vault.default.json`, mode 0600, plain JSON.
//! - `disabled`: no-op; reads return empty, writes are dropped.
//!
//! Each backend is an implementation of the `Storage` trait in its own
//! sibling file under `vault/`. This module owns the facade: the
//! `Vault` handle, substitution logic, the shared on-disk `Envelope`
//! format (so a plaintext-vs-encrypted mismatch is rejected before
//! anything writes), and the shared I/O helpers. Switching the
//! backend is one line in `settings.toml`; the rest of the pipeline
//! (`${VAR}` substitution, registry credential lookup, the `secret`
//! subcommand) is unaware.
//!
//! ## On-disk envelope
//!
//! ```json
//! { "type": "file",           "data": { ...VaultData... } }
//! { "type": "encrypted-file", "data": { "kdf": {...}, "nonce": "...", "ciphertext": "..." } }
//! ```
//!
//! ## Lazy opening
//!
//! `Vault::new()` does **not** touch storage. The first call to any
//! getter or setter opens it. For `encrypted-file` that's the call
//! that prompts for a passphrase; for `keyring` on Linux it's the call
//! that may trigger a Secret Service unlock. `Vault::subst` consults
//! the host-env snapshot first — a template like `${PATH}` resolves
//! without ever opening the vault, so only references to names that
//! the host env doesn't define fall through.
//!
//! ## Concurrency
//!
//! `Vault` guards its in-memory `VaultData` with a `Mutex<Option<_>>`
//! (`None` = unopened). Reads clone the needed fields out so the lock
//! is never held across foreign code. One `Vault` per process.
//!
//! Every write is a read-modify-write of the whole blob under a
//! cross-process lock file (`Storage::lock_path`, for every persistent
//! backend: `vault.default.lock`, `vault.default.enc.lock`,
//! `vault.keyring.lock`). So a writer never drops what another process
//! wrote, in any section. The vault lock is held only inside one vault
//! call, and no other lock is taken while it is held.
//!
//! The blob keeps top-level fields that this version does not know, so a
//! later format change survives a write by this version. One exception:
//! the `agents` section of an earlier unreleased version holds real agent
//! credentials that nothing reads any more, so the next write drops it
//! ([`RETIRED_AGENTS_SECTION`]).
//!
//! ## Error model
//!
//! "No vault yet" (file absent / no keyring entry) is not an error —
//! it's the initial state (empty vault). Everything else bubbles up
//! via `anyhow`. For `encrypted-file`, a wrong passphrase surfaces as
//! a decrypt error.

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
use encrypted::PassphraseSource;
use file::FileStorage;
use keyring::KeyringStorage;
use parking_lot::{Mutex, MutexGuard};
use rand::TryRng as _;
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

// Argon2id parameters — OWASP 2023 "second recommendation": 19 MiB
// memory, t=2, p=1. These land on the fast side of safe for an
// interactive unlock on a laptop (~100-300 ms).
pub(crate) const ARGON2_M_KIB: u32 = 19_456;
pub(crate) const ARGON2_T: u32 = 2;
pub(crate) const ARGON2_P: u32 = 1;
pub(crate) const ARGON2_KEY_BYTES: usize = 32;
pub(crate) const SALT_BYTES: usize = 16;
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

/// Metadata returned by `list_secrets`. Values are omitted; `preview`
/// is a short masked suffix (`****` plus 0/2/4 trailing chars depending
/// on value length) intended only for disambiguating similarly-named
/// entries. See `secret_preview`.
#[derive(Clone, Debug)]
pub struct SecretMeta {
    pub name: String,
    pub saved_at: SystemTime,
    pub preview: String,
}

/// Masked preview of a secret value, safe to show alongside its name.
/// Always prefixed with `****` so total length doesn't leak. Reveals
/// the last 4 chars when the value is ≥16 chars, the last 2 when ≥8,
/// nothing shorter — below 8 chars even two leaked chars are a
/// material fraction of the secret's entropy.
pub fn secret_preview(value: &str) -> String {
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

/// Plain registry credentials, decoupled from storage so callers can
/// construct them without touching internal entry types.
#[derive(Clone, Debug)]
pub struct RegistryCreds {
    pub username: String,
    pub password: String,
}

/// Top-level field of the agent credentials of an earlier unreleased
/// version (`airlock agents`). Dropped on the next write: it holds real
/// tokens and keys that no command can show or remove.
const RETIRED_AGENTS_SECTION: &str = "agents";

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct VaultData {
    #[serde(default)]
    secrets: BTreeMap<String, SecretEntry>,
    #[serde(default)]
    registries: BTreeMap<String, RegistryEntry>,
    /// Key of the network services' token store (in `~/.airlock/db/`):
    /// 32 random bytes, base64. Created once; not a user secret, so
    /// `airlock secrets` does not list it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_store_key: Option<String>,
    /// Top-level fields of a later format, kept through a write.
    #[serde(flatten)]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// Which backend `Vault` uses. Matches `settings.vault.storage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VaultStorageType {
    /// Inert backend. Reads empty, writes dropped. `airlock secrets`
    /// refuses to run.
    Disabled,
    /// Plaintext JSON at `~/.airlock/vault.default.json` (mode 0600).
    File,
    /// AEAD-encrypted JSON at `~/.airlock/vault.default.enc.json`. Passphrase via
    /// `AIRLOCK_VAULT_PASSPHRASE` or interactive prompt.
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

/// Process-global vault handle. Cheap to clone (internal `Arc`). One
/// per process is the expected usage.
#[derive(Clone)]
pub struct Vault {
    inner: Arc<VaultInner>,
}

struct VaultInner {
    storage: Box<dyn Storage>,
    data: Mutex<Option<VaultData>>,
    /// Host environment snapshot for `${NAME}` substitution. Frozen so
    /// tests can inject a known env and so substitution stays
    /// deterministic even if something else mutates `std::env` mid-run.
    env: HashMap<String, String>,
    /// Which backend this vault was constructed with — surfaced so the
    /// CLI can warn the user when they `secret add` into a plaintext
    /// file.
    storage_type: VaultStorageType,
}

impl Default for Vault {
    fn default() -> Self {
        Self::for_storage_type(VaultStorageType::default())
    }
}

impl Vault {
    /// Construct a vault for the given storage backend, reading the
    /// host environment snapshot now.
    pub fn for_storage_type(storage_type: VaultStorageType) -> Self {
        Self::new_with(
            boxed_storage(storage_type),
            std::env::vars().collect(),
            storage_type,
        )
    }

    /// Build a vault against a custom storage backend and a fixed env
    /// map. Intended for tests; the real CLI uses `Vault::for_storage_type`.
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

    /// Which backend this vault uses. Surfaces the active selection so
    /// subcommands can specialize (e.g. `secret add` warns on `File`).
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

    /// Read the current blob from storage (empty when there is none yet).
    fn load(&self) -> anyhow::Result<VaultData> {
        match self.inner.storage.load()? {
            Some(json) => serde_json::from_str::<VaultData>(&json)
                .context("parse airlock vault blob — storage may be corrupt"),
            None => Ok(VaultData::default()),
        }
    }

    /// Take the cross-process vault lock of the backend, if it has one.
    /// Held until the returned handle drops.
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

    /// Perform a mutation as a locked read-modify-write against the *current*
    /// on-disk state.
    ///
    /// The old approach mutated a possibly-stale cached snapshot and flushed
    /// it wholesale, so a long-running process could erase secrets a
    /// concurrent `airlock secrets add` had written. Here we take a
    /// cross-process lock (every persistent backend), reload the latest
    /// state, apply `f`, write it, and refresh the cache — so concurrent
    /// changes are merged rather than clobbered. When `f` fails, nothing
    /// is written.
    fn mutate<R>(&self, f: impl FnOnce(&mut VaultData) -> anyhow::Result<R>) -> anyhow::Result<R> {
        let _lock = self.lock_storage()?;
        let mut data = self.load()?;
        data.unknown.remove(RETIRED_AGENTS_SECTION);
        let result = f(&mut data)?;
        self.flush(&data)?;
        *self.inner.data.lock() = Some(data);
        Ok(result)
    }

    /// Lookup a user secret by name. Opens the vault on first use.
    pub fn get_secret(&self, name: &str) -> anyhow::Result<Option<String>> {
        let opened = self.open()?;
        Ok(opened.data().secrets.get(name).map(|e| e.value.clone()))
    }

    /// Store or overwrite a user secret. Rejects empty names/values
    /// and names that can't be used as env-var identifiers.
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

    /// Remove a user secret. `Ok(false)` when the name was not present
    /// — lets the CLI report "nothing to do" without conflating it
    /// with real storage errors.
    pub fn remove_secret(&self, name: &str) -> anyhow::Result<bool> {
        self.mutate(|data| Ok(data.secrets.remove(name).is_some()))
    }

    /// Enumerate secrets (names, timestamps, masked previews — no
    /// full values).
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

    /// Lookup registry credentials for `host`.
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

    /// The key of the network services' token store. Created on first use
    /// under the vault lock, so concurrent first uses agree on one key.
    pub fn service_store_key(&self) -> anyhow::Result<[u8; 32]> {
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

    /// Expand `${NAME}` tokens in `template`. Host env is consulted
    /// first and the vault is the fallback — so common templates like
    /// `${PATH}` or `${HOME}` never hit the vault.
    pub fn subst(&self, template: &str) -> anyhow::Result<String> {
        subst::substitute(template, self).map_err(|e| match self.open() {
            // The variable is missing because the vault did not open: say
            // why, not only that it is missing.
            Err(open) => open.context(e.to_string()),
            Ok(_) => anyhow!("{e}"),
        })
    }
}

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

/// Validate a user-secret name: must parse as a POSIX env-var
/// identifier (`[A-Z_][A-Z0-9_]*`). Names that can't be referenced
/// via `${NAME}` would be unreachable anyway.
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

/// Backend that persists the vault JSON blob. Vault hands the trait a
/// plain `VaultData` JSON string and takes the same back on load — any
/// on-disk envelope or encryption is the backend's concern.
pub trait Storage: Send + Sync + 'static {
    fn load(&self) -> anyhow::Result<Option<String>>;
    fn store(&self, data: &str) -> anyhow::Result<()>;

    /// Path of a sidecar lock file used to serialize concurrent mutations
    /// across processes. `None` for backends that don't need it
    /// (disabled, in-memory test doubles). An error when the backend needs
    /// a lock but cannot name its path: a write never runs unlocked.
    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }
}

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
// Both file backends share a tagged envelope so a `settings.vault` flip
// refuses to reinterpret one kind of file as the other rather than
// silently zeroing a vault. Defined here (not in `encrypted.rs`) so
// `file.rs` can match on it without a sibling-module import.

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub(crate) enum Envelope {
    File(VaultData),
    EncryptedFile(EncryptedBlob),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct EncryptedBlob {
    pub(crate) kdf: KdfParams,
    /// 12-byte ChaCha20-Poly1305 nonce, base64 (unpadded).
    pub(crate) nonce: String,
    /// AEAD ciphertext + 16-byte tag, base64 (unpadded).
    pub(crate) ciphertext: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct KdfParams {
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

pub(crate) fn read_vault_file(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("read vault file {}: {e}", path.display())),
    }
}

/// Write `bytes` to `path` atomically and with mode 0600. Goes via a
/// sibling tempfile + rename so a crash mid-write can't leave the
/// vault truncated. The parent directory is created if missing.
/// Acquire an exclusive advisory lock on `path`, held until the returned
/// handle drops. Blocking (mutations are brief), so concurrent writers queue
/// rather than fail. Used to serialize vault read-modify-write across
/// processes so a stale writer can't clobber a concurrent one's changes.
fn acquire_file_lock(path: &Path) -> anyhow::Result<File> {
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create vault directory {}", parent.display()))?;
    }
    // `O_NOFOLLOW`: a symlink planted at the lock path is refused, not
    // followed to create or lock some other file.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open vault lock {}", path.display()))?;
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

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create vault directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("vault path has no file name: {}", path.display()))?;
    // Per-process unique temp name so two processes writing concurrently can't
    // rename each other's half-written temp file into place.
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

pub(crate) fn decode_b64_array<const N: usize>(s: &str, label: &str) -> anyhow::Result<[u8; N]> {
    let bytes = STANDARD_NO_PAD
        .decode(s)
        .with_context(|| format!("decode vault {label}"))?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow!("vault {label} has wrong length: expected {N} bytes"))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn secret_preview_hides_short_values() {
        assert_eq!(secret_preview(""), "****");
        assert_eq!(secret_preview("abc"), "****");
        // 7 chars — still below the 8-char threshold.
        assert_eq!(secret_preview("abcdefg"), "****");
    }

    #[test]
    fn secret_preview_reveals_two_chars_between_8_and_15() {
        assert_eq!(secret_preview("abcdefgh"), "****gh");
        assert_eq!(secret_preview("password1234567"), "****67");
    }

    #[test]
    fn secret_preview_reveals_four_chars_at_16_and_above() {
        assert_eq!(secret_preview("abcdefghijklmnop"), "****mnop");
        assert_eq!(secret_preview("sk_live_abcdefghijklmnopqrstuv"), "****stuv");
    }

    #[test]
    fn secret_preview_counts_codepoints_not_bytes() {
        // 16 codepoints, each 2 bytes in UTF-8 — triggers the 4-char
        // branch on codepoint count, not the 32-byte total.
        let value: String = "ñ".repeat(16);
        assert_eq!(secret_preview(&value), "****ññññ");
    }

    /// In-memory `Storage` double for tests.
    #[derive(Default)]
    struct FakeStorage {
        blob: StdMutex<Option<String>>,
    }

    impl Storage for FakeStorage {
        fn load(&self) -> anyhow::Result<Option<String>> {
            Ok(self.blob.lock().unwrap().clone())
        }
        fn store(&self, data: &str) -> anyhow::Result<()> {
            *self.blob.lock().unwrap() = Some(data.to_string());
            Ok(())
        }
    }

    struct SharedStorage(Arc<FakeStorage>);

    impl Storage for SharedStorage {
        fn load(&self) -> anyhow::Result<Option<String>> {
            self.0.load()
        }
        fn store(&self, data: &str) -> anyhow::Result<()> {
            self.0.store(data)
        }
    }

    fn vault_with(storage: impl Storage, env: &[(&str, &str)]) -> Vault {
        Vault::new_with(
            Box::new(storage),
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            VaultStorageType::File,
        )
    }

    fn fresh_tmp(name: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("airlock-vault-test-{ts}-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    struct FixedPassphrase(&'static str);
    impl PassphraseSource for FixedPassphrase {
        fn unlock(&self) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
        fn create(&self) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    #[test]
    fn validate_secret_name_accepts_env_style_names() {
        assert!(validate_secret_name("FOO").is_ok());
        assert!(validate_secret_name("FOO_BAR").is_ok());
        assert!(validate_secret_name("_UNDERSCORE_START").is_ok());
        assert!(validate_secret_name("A1").is_ok());
        assert!(validate_secret_name("A_1_B").is_ok());
    }

    #[test]
    fn validate_secret_name_rejects_bad_names() {
        assert!(validate_secret_name("").is_err());
        assert!(validate_secret_name("foo").is_err());
        assert!(validate_secret_name("1X").is_err());
        assert!(validate_secret_name("A-B").is_err());
        assert!(validate_secret_name("A.B").is_err());
        assert!(validate_secret_name("A B").is_err());
    }

    #[test]
    fn secret_roundtrip_through_fake_storage() {
        let vault = vault_with(FakeStorage::default(), &[]);
        vault.set_secret("DATABASE_URL", "value-1").unwrap();
        assert_eq!(
            vault.get_secret("DATABASE_URL").unwrap(),
            Some("value-1".to_string())
        );
        vault.set_secret("DATABASE_URL", "value-2").unwrap();
        assert_eq!(
            vault.get_secret("DATABASE_URL").unwrap(),
            Some("value-2".to_string())
        );
        assert!(vault.remove_secret("DATABASE_URL").unwrap());
        assert!(vault.get_secret("DATABASE_URL").unwrap().is_none());
        assert!(!vault.remove_secret("DATABASE_URL").unwrap());
    }

    /// `Vault::for_storage_type(Disabled)` must construct without touching any
    /// backend — preserves the zero-config "airlock just works" path.
    #[test]
    fn construction_does_not_touch_storage() {
        struct PanickingStorage;
        impl Storage for PanickingStorage {
            fn load(&self) -> anyhow::Result<Option<String>> {
                panic!("load called during construction");
            }
            fn store(&self, _: &str) -> anyhow::Result<()> {
                panic!("store called during construction");
            }
        }
        let _ = Vault::new_with(
            Box::new(PanickingStorage),
            HashMap::new(),
            VaultStorageType::File,
        );
    }

    #[test]
    fn second_vault_sees_flushed_writes() {
        let shared = Arc::new(FakeStorage::default());
        let a = vault_with(SharedStorage(shared.clone()), &[]);
        a.set_secret("TOKEN", "abc123").unwrap();
        a.set_registry(
            "ghcr.io",
            &RegistryCreds {
                username: "alice".to_string(),
                password: "hunter2".to_string(),
            },
        )
        .unwrap();
        let b = vault_with(SharedStorage(shared), &[]);
        assert_eq!(b.get_secret("TOKEN").unwrap(), Some("abc123".to_string()));
        let creds = b.get_registry("ghcr.io").unwrap().unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "hunter2");
    }

    #[test]
    fn the_service_store_key_is_created_once_and_hidden() {
        let shared = Arc::new(FakeStorage::default());
        let a = vault_with(SharedStorage(shared.clone()), &[]);
        let key = a.service_store_key().unwrap();
        assert_eq!(a.service_store_key().unwrap(), key);
        let b = vault_with(SharedStorage(shared), &[]);
        assert_eq!(
            b.service_store_key().unwrap(),
            key,
            "stored, not regenerated"
        );
        assert!(b.list_secrets().unwrap().is_empty(), "not a user secret");
    }

    #[test]
    fn set_secret_rejects_empty_and_bad_names() {
        let vault = vault_with(FakeStorage::default(), &[]);
        assert!(vault.set_secret("FOO", "").is_err());
        assert!(vault.set_secret("lowercase", "x").is_err());
    }

    #[test]
    fn subst_literal_skips_vault_open() {
        struct PanickingStorage;
        impl Storage for PanickingStorage {
            fn load(&self) -> anyhow::Result<Option<String>> {
                panic!("vault should not open for a literal template");
            }
            fn store(&self, _: &str) -> anyhow::Result<()> {
                panic!("vault should not write for a literal template");
            }
        }
        let vault = Vault::new_with(
            Box::new(PanickingStorage),
            HashMap::new(),
            VaultStorageType::File,
        );
        assert_eq!(vault.subst("plain-value").unwrap(), "plain-value");
        assert_eq!(vault.subst("").unwrap(), "");
    }

    #[test]
    fn subst_resolves_from_env() {
        let vault = vault_with(FakeStorage::default(), &[("HOME_DIR", "/home/alice")]);
        assert_eq!(
            vault.subst("prefix:${HOME_DIR}/suffix").unwrap(),
            "prefix:/home/alice/suffix"
        );
    }

    #[test]
    fn subst_resolves_from_vault() {
        let vault = vault_with(FakeStorage::default(), &[]);
        vault.set_secret("DATABASE_URL", "postgres://db").unwrap();
        assert_eq!(
            vault.subst("url=${DATABASE_URL}").unwrap(),
            "url=postgres://db"
        );
    }

    #[test]
    fn subst_env_wins_over_vault() {
        let vault = vault_with(FakeStorage::default(), &[("TOKEN", "from-env")]);
        vault.set_secret("TOKEN", "from-vault").unwrap();
        assert_eq!(vault.subst("${TOKEN}").unwrap(), "from-env");
    }

    #[test]
    fn subst_missing_variable_errors() {
        let vault = vault_with(FakeStorage::default(), &[]);
        assert!(vault.subst("${NOPE}").is_err());
    }

    /// A vault that fails to open is an error for `${NAME}`, not an unset
    /// variable: the user must see why the secret is missing.
    #[test]
    fn subst_surfaces_open_errors() {
        struct BrokenStorage;
        impl Storage for BrokenStorage {
            fn load(&self) -> anyhow::Result<Option<String>> {
                anyhow::bail!("keyring locked")
            }
            fn store(&self, _data: &str) -> anyhow::Result<()> {
                anyhow::bail!("keyring locked")
            }
        }
        let vault = vault_with(BrokenStorage, &[("HOST", "from-env")]);
        assert_eq!(vault.subst("${HOST}").unwrap(), "from-env");
        let err = vault.subst("${TOKEN}").unwrap_err();
        assert!(format!("{err:#}").contains("keyring locked"), "{err:#}");
    }

    #[test]
    fn subst_mixes_vault_and_env_in_one_template() {
        let vault = vault_with(FakeStorage::default(), &[("USER", "alice")]);
        vault.set_secret("TOKEN", "s3cret").unwrap();
        assert_eq!(vault.subst("${USER}:${TOKEN}").unwrap(), "alice:s3cret");
    }

    // ── File storage ─────────────────────────────────────────────────────

    #[test]
    fn file_storage_roundtrip() {
        let path = fresh_tmp("vault.json");
        let storage = FileStorage::new(path.clone());
        let vault = Vault::new_with(Box::new(storage), HashMap::new(), VaultStorageType::File);

        vault.set_secret("TOKEN", "abc").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"type\": \"file\""), "{raw}");

        let vault2 = Vault::new_with(
            Box::new(FileStorage::new(path)),
            HashMap::new(),
            VaultStorageType::File,
        );
        assert_eq!(vault2.get_secret("TOKEN").unwrap(), Some("abc".to_string()));
    }

    /// Two independent vault handles on the same file (as two processes would
    /// be) must not lose each other's secrets: a write from one merges onto
    /// the other's concurrent write instead of clobbering it.
    #[test]
    fn concurrent_writers_do_not_lose_secrets() {
        let path = fresh_tmp("shared.json");
        let a = Vault::new_with(
            Box::new(FileStorage::new(path.clone())),
            HashMap::new(),
            VaultStorageType::File,
        );
        let b = Vault::new_with(
            Box::new(FileStorage::new(path.clone())),
            HashMap::new(),
            VaultStorageType::File,
        );

        // `a` opens (caching an empty snapshot), then `b` adds a secret, then
        // `a` adds a different one. The old code flushed a's stale snapshot and
        // erased b's write; now a re-reads and merges.
        assert!(a.list_secrets().unwrap().is_empty());
        b.set_secret("FROM_B", "1").unwrap();
        a.set_secret("FROM_A", "2").unwrap();

        let fresh = Vault::new_with(
            Box::new(FileStorage::new(path)),
            HashMap::new(),
            VaultStorageType::File,
        );
        assert_eq!(fresh.get_secret("FROM_A").unwrap(), Some("2".to_string()));
        assert_eq!(fresh.get_secret("FROM_B").unwrap(), Some("1".to_string()));
    }

    /// Missing file → empty vault (not an error).
    #[test]
    fn file_storage_missing_file_is_empty() {
        let path = fresh_tmp("never-written.json");
        let vault = Vault::new_with(
            Box::new(FileStorage::new(path)),
            HashMap::new(),
            VaultStorageType::File,
        );
        assert!(vault.list_secrets().unwrap().is_empty());
    }

    /// A file written by the encrypted backend must not silently open
    /// as a plain-file vault (data loss).
    #[test]
    fn file_storage_rejects_encrypted_envelope() {
        let path = fresh_tmp("vault.json");
        EncryptedFileStorage::new(path.clone(), Box::new(FixedPassphrase("pw")))
            .store(r#"{"secrets":{},"registries":{}}"#)
            .unwrap();
        let err = FileStorage::new(path).load().unwrap_err();
        assert!(err.to_string().contains("encrypted vault"), "got: {err:#}");
    }

    // ── Encrypted-file storage ──────────────────────────────────────────

    #[test]
    fn encrypted_file_storage_roundtrip() {
        let path = fresh_tmp("vault.json");
        let storage = EncryptedFileStorage::new(path.clone(), Box::new(FixedPassphrase("hunter2")));
        let vault = Vault::new_with(
            Box::new(storage),
            HashMap::new(),
            VaultStorageType::EncryptedFile,
        );

        vault.set_secret("TOKEN", "abc").unwrap();
        vault.set_secret("OTHER", "xyz").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"type\": \"encrypted-file\""), "{raw}");
        assert!(!raw.contains("abc"), "ciphertext leaked plaintext: {raw}");

        let vault2 = Vault::new_with(
            Box::new(EncryptedFileStorage::new(
                path,
                Box::new(FixedPassphrase("hunter2")),
            )),
            HashMap::new(),
            VaultStorageType::EncryptedFile,
        );
        assert_eq!(vault2.get_secret("TOKEN").unwrap(), Some("abc".to_string()));
        assert_eq!(vault2.get_secret("OTHER").unwrap(), Some("xyz".to_string()));
    }

    #[test]
    fn encrypted_file_storage_rejects_wrong_passphrase() {
        let path = fresh_tmp("vault.json");
        let storage = EncryptedFileStorage::new(path.clone(), Box::new(FixedPassphrase("right")));
        let vault = Vault::new_with(
            Box::new(storage),
            HashMap::new(),
            VaultStorageType::EncryptedFile,
        );
        vault.set_secret("TOKEN", "abc").unwrap();

        let bad = Vault::new_with(
            Box::new(EncryptedFileStorage::new(
                path,
                Box::new(FixedPassphrase("wrong")),
            )),
            HashMap::new(),
            VaultStorageType::EncryptedFile,
        );
        let err = bad.get_secret("TOKEN").unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"), "got: {err:#}");
    }

    #[test]
    fn encrypted_file_storage_rejects_plaintext_envelope() {
        let path = fresh_tmp("vault.json");
        FileStorage::new(path.clone())
            .store(r#"{"secrets":{},"registries":{}}"#)
            .unwrap();
        let err = EncryptedFileStorage::new(path, Box::new(FixedPassphrase("pw")))
            .load()
            .unwrap_err();
        assert!(err.to_string().contains("plaintext vault"), "got: {err:#}");
    }

    // ── Vault lock ───────────────────────────────────────────────────────

    /// One stored blob with a lock file, as the keyring backend of two
    /// processes. With `stall`, the first `store` reports that it runs
    /// (the writer holds the vault lock), then waits before it writes: a
    /// writer that did not wait for the lock would load the old blob in
    /// that window and drop this write.
    struct LockedStorage {
        blob: Arc<FakeStorage>,
        lock: PathBuf,
        stall: StdMutex<Option<std::sync::mpsc::Sender<()>>>,
    }

    impl Storage for LockedStorage {
        fn load(&self) -> anyhow::Result<Option<String>> {
            self.blob.load()
        }
        fn store(&self, data: &str) -> anyhow::Result<()> {
            if let Some(in_store) = self.stall.lock().unwrap().take() {
                in_store.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            self.blob.store(data)
        }
        fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
            Ok(Some(self.lock.clone()))
        }
    }

    #[test]
    fn concurrent_writers_with_the_vault_lock_both_survive() {
        let blob = Arc::new(FakeStorage::default());
        let lock = fresh_tmp("vault.keyring.lock");
        let (in_store, stalled) = std::sync::mpsc::channel();
        let a = vault_with(
            LockedStorage {
                blob: blob.clone(),
                lock: lock.clone(),
                stall: StdMutex::new(Some(in_store)),
            },
            &[],
        );
        let b = vault_with(
            LockedStorage {
                blob: blob.clone(),
                lock,
                stall: StdMutex::new(None),
            },
            &[],
        );
        let writer_a = std::thread::spawn(move || a.set_secret("A", "1"));
        stalled.recv().unwrap();
        b.set_secret("B", "2").unwrap();
        writer_a.join().unwrap().unwrap();

        let fresh = vault_with(SharedStorage(blob), &[]);
        assert_eq!(fresh.get_secret("A").unwrap(), Some("1".to_string()));
        assert_eq!(fresh.get_secret("B").unwrap(), Some("2".to_string()));
    }

    /// A symlink at the lock path is refused: the lock never follows it
    /// to create or lock another file.
    #[test]
    fn the_vault_lock_refuses_a_symlink() {
        let lock = fresh_tmp("vault.keyring.lock");
        let target = lock.with_file_name("elsewhere");
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        let vault = vault_with(
            LockedStorage {
                blob: Arc::new(FakeStorage::default()),
                lock,
                stall: StdMutex::new(None),
            },
            &[],
        );
        let err = vault.set_secret("A", "1").unwrap_err();
        assert!(format!("{err:#}").contains("open vault lock"), "{err:#}");
        assert!(!target.exists());
        assert!(vault.get_secret("A").unwrap().is_none());
    }

    /// A backend that cannot name its lock path does not write unlocked.
    #[test]
    fn a_lock_path_error_stops_the_write() {
        struct NoLockPath(Arc<FakeStorage>);
        impl Storage for NoLockPath {
            fn load(&self) -> anyhow::Result<Option<String>> {
                self.0.load()
            }
            fn store(&self, data: &str) -> anyhow::Result<()> {
                self.0.store(data)
            }
            fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
                anyhow::bail!("no home directory")
            }
        }
        let blob = Arc::new(FakeStorage::default());
        let vault = vault_with(NoLockPath(blob.clone()), &[]);
        assert!(vault.set_secret("A", "1").is_err());
        assert!(blob.load().unwrap().is_none());
    }

    /// Top-level fields that this version does not know survive a write
    /// untouched, and they are not secrets. The retired `agents` section
    /// is the exception: a write drops it.
    #[test]
    fn unknown_top_level_fields_survive_a_write() {
        let later = serde_json::json!({"x": 1});
        let storage = Arc::new(FakeStorage::default());
        storage
            .store(r#"{"secrets":{},"later":{"x":1},"agents":{"claude":{"record":{"version":1}}}}"#)
            .unwrap();
        let vault = vault_with(SharedStorage(storage.clone()), &[]);
        vault.set_secret("TOKEN", "abc").unwrap();
        let names: Vec<String> = vault
            .list_secrets()
            .unwrap()
            .into_iter()
            .map(|meta| meta.name)
            .collect();
        assert_eq!(names, ["TOKEN"]);
        assert!(!vault.remove_secret("later").unwrap());
        let blob: serde_json::Value =
            serde_json::from_str(&storage.load().unwrap().unwrap()).unwrap();
        assert_eq!(blob["later"], later);
        assert!(blob.get(RETIRED_AGENTS_SECTION).is_none());
        assert!(blob["secrets"]["TOKEN"].is_object());

        // The file backend re-serializes the blob through its envelope.
        let path = fresh_tmp("vault.json");
        std::fs::write(
            &path,
            r#"{"type":"file","data":{"secrets":{},"future":{"x":1}}}"#,
        )
        .unwrap();
        let vault = Vault::new_with(
            Box::new(FileStorage::new(path.clone())),
            HashMap::new(),
            VaultStorageType::File,
        );
        vault.set_secret("TOKEN", "abc").unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["data"]["future"], serde_json::json!({"x": 1}));
        assert!(raw["data"]["secrets"]["TOKEN"].is_object());
    }
}
