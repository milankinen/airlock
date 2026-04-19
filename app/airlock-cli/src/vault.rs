//! Secret storage for airlock.
//!
//! Holds two kinds of items:
//!
//! - `secrets`: user-managed secrets (`airlock secret add/ls/rm`)
//!   exposed to projects via `${NAME}` substitution.
//! - `registries`: image-registry credentials.
//!
//! Both kinds live inside a **single** `VaultData` blob. Where that blob
//! lives is chosen by `settings.vault`:
//!
//! - `encrypted-file` (default): `~/.airlock/vault.default.enc.json`, AEAD-encrypted with a passphrase.
//! - `file`: `~/.airlock/vault.default.json`, mode 0600, plain JSON.
//! - `keyring`: OS keychain / Secret Service (the pre-existing backend).
//! - `disabled`: no-op; reads return empty, writes are dropped.
//!
//! `Vault::new()` delegates to a `Storage` trait object — each variant is
//! an implementation of that trait. Switching the backend is one line
//! in `settings.toml`; the rest of the pipeline (`${VAR}` substitution,
//! registry credential lookup, the `secret` subcommand) is unaware.
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
//! ## Error model
//!
//! "No vault yet" (file absent / no keyring entry) is not an error —
//! it's the initial state (empty vault). Everything else bubbles up
//! via `anyhow`. For `encrypted-file`, a wrong passphrase surfaces as
//! a decrypt error.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use parking_lot::{Mutex, MutexGuard};
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

const KEYRING_SERVICE: &str = "airlock-vault";
const KEYRING_ACCOUNT: &str = "default";

/// Env var used to supply the encrypted-vault passphrase non-interactively
/// (CI, scripts, and headless shells without a TTY).
const PASSPHRASE_ENV: &str = "AIRLOCK_VAULT_PASSPHRASE";

// Argon2id parameters — OWASP 2023 "second recommendation": 19 MiB
// memory, t=2, p=1. These land on the fast side of safe for an
// interactive unlock on a laptop (~100-300 ms).
const ARGON2_M_KIB: u32 = 19_456;
const ARGON2_T: u32 = 2;
const ARGON2_P: u32 = 1;
const ARGON2_KEY_BYTES: usize = 32;
const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 12;

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

/// Metadata returned by `list_secrets` — values intentionally omitted.
#[derive(Clone, Debug)]
pub struct SecretMeta {
    pub name: String,
    pub saved_at: SystemTime,
}

/// Plain registry credentials, decoupled from storage so callers can
/// construct them without touching internal entry types.
#[derive(Clone, Debug)]
pub struct RegistryCreds {
    pub username: String,
    pub password: String,
}

#[derive(Default, Serialize, Deserialize)]
struct VaultData {
    #[serde(default)]
    secrets: BTreeMap<String, SecretEntry>,
    #[serde(default)]
    registries: BTreeMap<String, RegistryEntry>,
}

/// Which backend `Vault` uses. Matches `settings.vault.storage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VaultStorageType {
    /// Inert backend. Reads empty, writes dropped. `airlock secret`
    /// refuses to run.
    Disabled,
    /// Plaintext JSON at `~/.airlock/vault.default.json` (mode 0600).
    File,
    /// AEAD-encrypted JSON at `~/.airlock/vault.default.enc.json`. Passphrase via
    /// `AIRLOCK_VAULT_PASSPHRASE` or interactive prompt.
    #[default]
    EncryptedFile,
    /// OS keychain / Secret Service.
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
            let data = match self.inner.storage.load()? {
                Some(json) => serde_json::from_str::<VaultData>(&json)
                    .context("parse airlock vault blob — storage may be corrupt")?,
                None => VaultData::default(),
            };
            *guard = Some(data);
        }
        Ok(OpenedVault(guard))
    }

    fn flush(&self, data: &VaultData) -> anyhow::Result<()> {
        let json = serde_json::to_string(data).context("serialize airlock vault")?;
        self.inner.storage.store(&json)
    }

    /// Lookup a user secret by name. Opens the vault on first use.
    #[allow(dead_code)]
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
        let mut opened = self.open()?;
        opened.data_mut().secrets.insert(
            name.to_string(),
            SecretEntry {
                value: value.to_string(),
                saved_at: SystemTime::now(),
            },
        );
        self.flush(opened.data())
    }

    /// Remove a user secret. `Ok(false)` when the name was not present
    /// — lets the CLI report "nothing to do" without conflating it
    /// with real storage errors.
    pub fn remove_secret(&self, name: &str) -> anyhow::Result<bool> {
        let mut opened = self.open()?;
        let existed = opened.data_mut().secrets.remove(name).is_some();
        if existed {
            self.flush(opened.data())?;
        }
        Ok(existed)
    }

    /// Enumerate secrets (names + timestamps only — no values).
    pub fn list_secrets(&self) -> anyhow::Result<Vec<SecretMeta>> {
        let opened = self.open()?;
        Ok(opened
            .data()
            .secrets
            .iter()
            .map(|(name, entry)| SecretMeta {
                name: name.clone(),
                saved_at: entry.saved_at,
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
        let mut opened = self.open()?;
        opened.data_mut().registries.insert(
            host.to_string(),
            RegistryEntry {
                username: creds.username.clone(),
                password: creds.password.clone(),
                saved_at: SystemTime::now(),
            },
        );
        self.flush(opened.data())
    }

    /// Expand `${NAME}` tokens in `template`. Host env is consulted
    /// first and the vault is the fallback — so common templates like
    /// `${PATH}` or `${HOME}` never hit the vault.
    pub fn subst(&self, template: &str) -> anyhow::Result<String> {
        subst::substitute(template, self).map_err(|e| anyhow!("{e}"))
    }
}

struct OpenedVault<'a>(MutexGuard<'a, Option<VaultData>>);

impl<'a> subst::VariableMap<'a> for Vault {
    type Value = String;
    fn get(&'a self, key: &str) -> Option<Self::Value> {
        if let Some(value) = self.inner.env.get(key) {
            return Some(value.clone());
        }
        self.open()
            .ok()
            .and_then(|v| v.data().secrets.get(key).map(|s| s.value.clone()))
    }
}

impl OpenedVault<'_> {
    fn data(&self) -> &VaultData {
        self.0.as_ref().expect("opened vault has data")
    }

    fn data_mut(&mut self) -> &mut VaultData {
        self.0.as_mut().expect("opened vault has data")
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

// ── Storage trait + backends ────────────────────────────────────────────────

/// Backend that persists the vault JSON blob. Vault hands the trait a
/// plain `VaultData` JSON string and takes the same back on load — any
/// on-disk envelope or encryption is the backend's concern.
pub trait Storage: Send + Sync + 'static {
    fn load(&self) -> anyhow::Result<Option<String>>;
    fn store(&self, data: &str) -> anyhow::Result<()>;
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
            interactive_passphrase(),
        )),
        VaultStorageType::Keyring => Box::new(KeyringStorage),
    }
}

/// Tagged on-disk envelope for the file backends. `type` distinguishes
/// plaintext from encrypted so we can refuse to reinterpret one as the
/// other when the user flips the setting — better to fail loudly than
/// to silently zero out a vault.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
enum Envelope {
    File(VaultData),
    EncryptedFile(EncryptedBlob),
}

#[derive(Serialize, Deserialize)]
struct EncryptedBlob {
    kdf: KdfParams,
    /// 12-byte ChaCha20-Poly1305 nonce, base64 (unpadded).
    nonce: String,
    /// AEAD ciphertext + 16-byte tag, base64 (unpadded).
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct KdfParams {
    algo: String,
    /// 16-byte salt, base64 (unpadded).
    salt: String,
    /// Memory cost (KiB).
    m: u32,
    /// Time cost (iterations).
    t: u32,
    /// Parallelism.
    p: u32,
}

// ── File storage ────────────────────────────────────────────────────────────

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

// ── Encrypted-file storage ─────────────────────────────────────────────────

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

fn decode_b64_array<const N: usize>(s: &str, label: &str) -> anyhow::Result<[u8; N]> {
    let bytes = STANDARD_NO_PAD
        .decode(s)
        .with_context(|| format!("decode vault {label}"))?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow!("vault {label} has wrong length: expected {N} bytes"))
}

// ── Keyring storage ─────────────────────────────────────────────────────────

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
}

fn keyring_entry() -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("construct airlock keyring entry")
}

// ── Disabled storage ────────────────────────────────────────────────────────

pub struct DisabledStorage;

impl Storage for DisabledStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

// ── File I/O helpers ────────────────────────────────────────────────────────

fn read_vault_file(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("read vault file {}: {e}", path.display())),
    }
}

/// Write `bytes` to `path` atomically and with mode 0600. Goes via a
/// sibling tempfile + rename so a crash mid-write can't leave the
/// vault truncated. The parent directory is created if missing.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create vault directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("vault path has no file name: {}", path.display()))?;
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!("{}.tmp", file_name.to_string_lossy()));

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

// ── Interactive passphrase source ──────────────────────────────────────────

/// Production passphrase source: checks `AIRLOCK_VAULT_PASSPHRASE`
/// first (covers CI / non-interactive runs), then falls back to a
/// suppressed-echo terminal prompt. On successful input the prompt
/// line is erased so the terminal stays clean.
pub struct InteractivePassphrase;

fn interactive_passphrase() -> Box<dyn PassphraseSource> {
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

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
            .map(|d| d.as_nanos())
            .unwrap_or(0);
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
}
