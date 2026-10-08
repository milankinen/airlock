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
pub(crate) use encrypted::PassphraseSource;
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
mod tests;
