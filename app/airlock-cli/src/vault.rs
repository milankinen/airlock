//! System-keyring-backed secret storage for airlock.
//!
//! Holds two kinds of items:
//!
//! - `secrets`: user-managed secrets (`airlock secret add/ls/rm`)
//!   exposed to projects via `${NAME}` substitution.
//! - `registries`: image-registry credentials (previously a per-OS
//!   pile; unified here so there's one place to look).
//!
//! Both kinds live inside a **single** keyring entry. Its password is
//! a JSON blob of the whole vault. One read on first access, one
//! write per mutation — no sidecar index, so name enumeration can't
//! drift from actual stored values (the failure mode `rcman::list_keys`
//! exhibits).
//!
//! The blob sits in the OS keychain/secret service:
//!
//! - macOS: Keychain Services (via `apple-native`).
//! - Linux: Secret Service over D-Bus (via `sync-secret-service`).
//!
//! Windows is not a target.
//!
//! ## Lazy opening
//!
//! `Vault::new()` does **not** touch the keyring. The first call to any
//! getter or setter opens it, which is what actually triggers the OS
//! unlock prompt on Linux. This means `airlock` commands that don't
//! reference secrets (e.g. `airlock show` on a project with no env
//! substitution) never prompt at all. `Vault::subst` preserves this
//! by consulting the host-env snapshot first — a template like
//! `${PATH}` resolves without ever opening the vault, so only
//! references to names that the host env doesn't define fall through
//! to the keyring.
//!
//! ## In-memory lifetime of secret values
//!
//! Values are stored and returned as plain `String`. No zeroization:
//! the JSON blob, `keyring`'s internal buffers, and every serde
//! intermediate would also need to be zeroized to make it meaningful,
//! and the CLI is short-lived enough that the incremental defense
//! against post-process memory scraping isn't worth the complexity.
//! Anything with read access to this process's heap can already see
//! every secret.
//!
//! ## Concurrency
//!
//! `Vault` guards its in-memory state with a `Mutex<Option<VaultData>>`
//! (`None` = unopened). Each operation takes the lock, reads or mutates,
//! and — on writes — flushes the blob through the keyring before
//! dropping the lock. Reads return owned clones so the lock is never
//! held across foreign code. One `Vault` per process.
//!
//! ## Error model
//!
//! "Keyring has no entry" is not an error — it's the initial state
//! (empty vault). Any other keyring error bubbles up via `anyhow`.
//! Missing D-Bus / Secret Service on Linux surfaces as
//! `keyring::Error::PlatformFailure`; callers should treat it as
//! "secrets unavailable on this host".
//!

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, bail};
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};

/// Keyring service identifier — the "app name" under which every
/// airlock install stores its single vault entry.
const SERVICE: &str = "airlock-vault";

/// Keyring username/account identifier for the vault entry. Pairs
/// with `SERVICE` to form the unique keyring key.
const ACCOUNT: &str = "vault";

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

/// Plain registry credentials, decoupled from the keyring storage so
/// callers can construct them without touching the internal entry types.
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

/// Process-global vault handle. `Vault::new()` is cheap and doesn't
/// contact the keyring — each accessor opens it lazily on first use.
///
/// Clonable: all state lives behind an internal `Arc`, so clones share
/// the same keyring backend and in-memory cache. That way callers can
/// pass `Vault` by value without wrapping it in an outer `Arc`.
#[derive(Clone)]
pub struct Vault {
    inner: Arc<VaultInner>,
}

struct VaultInner {
    keyring: Box<dyn Keyring>,
    data: Mutex<Option<VaultData>>,
    /// Host environment snapshot, captured at construction. Used as a
    /// fallback source for `${NAME}` substitution (vault secrets win
    /// on collision). Frozen so tests can inject a known env without
    /// mutating the live process environment, and so substitution
    /// stays deterministic for the lifetime of the process even if
    /// something else changes `std::env` under us.
    env: HashMap<String, String>,
}

impl Default for Vault {
    fn default() -> Self {
        Self::new()
    }
}

impl Vault {
    /// Construct an unopened vault handle backed by the real system
    /// keyring. The first getter/setter call reads from that keyring
    /// — that is the call that may trigger an unlock prompt. The host
    /// environment is snapshotted now so substitution is stable for
    /// the rest of the process lifetime.
    pub fn new() -> Self {
        Self::new_with(DefaultKeyring, std::env::vars().collect())
    }

    /// Construct a vault that never talks to the keyring: reads yield
    /// `None`, writes are silently dropped. Used when
    /// `settings.vault_enabled = false` so the rest of the pipeline
    /// (substitution, registry auth) still has a `Vault` to call but
    /// no unlock prompts or sidecar state appear. Env substitution
    /// still works from the host-env snapshot.
    pub fn disabled() -> Self {
        Self::new_with(NoopKeyring, std::env::vars().collect())
    }

    /// Build a vault against a custom keyring backend and a fixed
    /// env-var map. Intended for tests — the real CLI uses
    /// `Vault::new()`. Supplying the env explicitly makes substitution
    /// tests deterministic without mutating `std::env`.
    pub fn new_with(keyring: impl Keyring, env: HashMap<String, String>) -> Self {
        Self {
            inner: Arc::new(VaultInner {
                keyring: Box::new(keyring),
                data: Mutex::new(None),
                env,
            }),
        }
    }

    /// Open the vault: fetch the blob from the keyring and parse it,
    /// or start from an empty `VaultData` if there is no stored blob.
    /// Idempotent — once opened, subsequent calls just re-take the
    /// lock. Returns an `OpenedVault` whose drop releases the lock.
    fn open(&self) -> anyhow::Result<OpenedVault<'_>> {
        let mut guard = self.inner.data.lock();
        if guard.is_none() {
            let data = match self.inner.keyring.load()? {
                Some(json) => serde_json::from_str::<VaultData>(&json)
                    .context("parse airlock vault blob — storage may be corrupt")?,
                None => VaultData::default(),
            };
            *guard = Some(data);
        }
        Ok(OpenedVault(guard))
    }

    /// Serialize the current in-memory state and push it to the keyring.
    fn flush(&self, data: &VaultData) -> anyhow::Result<()> {
        let json = serde_json::to_string(data).context("serialize airlock vault")?;
        self.inner.keyring.store(&json)
    }

    /// Lookup a user secret by name. Opens the vault on first use.
    /// Template substitution goes through `Vault::subst` instead; this
    /// stays as the natural single-name accessor for future features
    /// and the unit tests.
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

    /// Remove a user secret. Returns `Ok(false)` when the name was
    /// not present — this lets the CLI report "nothing to do" without
    /// conflating it with real storage errors.
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

    /// Lookup registry credentials for `host`. Returns a plain
    /// `RegistryCreds` — the in-keyring `Zeroizing` wrapper is shed
    /// for caller ergonomics since the result is immediately handed
    /// to the OCI client.
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
    /// `${PATH}` or `${HOME}` never contact the keyring. Only a name
    /// the host env doesn't define falls through to the vault, which
    /// is the path that may prompt for keyring unlock.
    pub fn subst(&self, template: &str) -> anyhow::Result<String> {
        subst::substitute(template, self).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// A handle to the opened vault. Holds the `Mutex` guard so callers
/// have exclusive access to `VaultData` for the duration of one
/// operation; dropping it releases the lock.
struct OpenedVault<'a>(MutexGuard<'a, Option<VaultData>>);

impl<'a> subst::VariableMap<'a> for Vault {
    type Value = String;
    fn get(&'a self, key: &str) -> Option<Self::Value> {
        // Host env first so the common case (`${PATH}`, `${HOME}`, ...)
        // resolves without ever contacting the keyring. Only fall back
        // to the vault when env has no match — that's the path that
        // may open the vault and potentially prompt for unlock.
        if let Some(value) = self.inner.env.get(key) {
            return Some(value.clone());
        }
        self.open()
            .ok()
            .and_then(|v| v.data().secrets.get(key).map(|s| s.value.clone()))
    }
}

impl OpenedVault<'_> {
    /// Reference into the decoded vault data. Safe to `unwrap` because
    /// `Vault::open` populates the `Option` before returning.
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

/// Backend for storing/retrieving the vault blob. The production
/// implementation is `RealKeyring` (OS keychain / Secret Service); tests
/// plug in an in-memory fake to exercise vault logic without touching
/// the real keyring.
pub trait Keyring: Send + Sync + 'static {
    /// Return the stored blob, or `None` if no vault has been saved yet.
    fn load(&self) -> anyhow::Result<Option<String>>;
    /// Persist the blob, overwriting any previous value.
    fn store(&self, data: &str) -> anyhow::Result<()>;
}

struct DefaultKeyring;

impl Keyring for DefaultKeyring {
    fn load(&self) -> anyhow::Result<Option<String>> {
        match entry()?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("read airlock vault from keyring: {e}")),
        }
    }

    fn store(&self, data: &str) -> anyhow::Result<()> {
        entry()?
            .set_password(data)
            .context("write airlock vault to keyring")
    }
}

fn entry() -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT).context("construct airlock keyring entry")
}

/// Inert keyring backend. `load` always returns `Ok(None)` (empty
/// vault) and `store` is a no-op. Used by `Vault::disabled()` so the
/// secret vault can be turned off without plumbing a separate
/// "is-enabled" flag through every accessor.
struct NoopKeyring;

impl Keyring for NoopKeyring {
    fn load(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn store(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    /// In-memory `Keyring` double used by tests. Holds the "stored"
    /// blob behind a mutex so the `&self` trait methods can mutate it.
    #[derive(Default)]
    struct FakeKeyring {
        blob: StdMutex<Option<String>>,
    }

    impl Keyring for FakeKeyring {
        fn load(&self) -> anyhow::Result<Option<String>> {
            Ok(self.blob.lock().unwrap().clone())
        }
        fn store(&self, data: &str) -> anyhow::Result<()> {
            *self.blob.lock().unwrap() = Some(data.to_string());
            Ok(())
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
        assert!(validate_secret_name("foo").is_err(), "lowercase rejected");
        assert!(validate_secret_name("1X").is_err(), "digit-first rejected");
        assert!(validate_secret_name("A-B").is_err(), "dash rejected");
        assert!(validate_secret_name("A.B").is_err(), "dot rejected");
        assert!(validate_secret_name("A B").is_err(), "space rejected");
    }

    /// JSON serialization must round-trip a full vault. Guards against
    /// silent schema drift if `SecretEntry` or `RegistryEntry` gain
    /// fields without `#[serde(default)]`.
    #[test]
    fn vault_data_roundtrips_json() {
        let mut data = VaultData::default();
        data.secrets.insert(
            "DATABASE_URL".to_string(),
            SecretEntry {
                value: "postgres://...".to_string(),
                saved_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            },
        );
        data.registries.insert(
            "ghcr.io".to_string(),
            RegistryEntry {
                username: "alice".to_string(),
                password: "hunter2".to_string(),
                saved_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            },
        );
        let json = serde_json::to_string(&data).unwrap();
        let back: VaultData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.secrets.len(), 1);
        assert_eq!(
            back.secrets.get("DATABASE_URL").unwrap().value,
            "postgres://..."
        );
        assert_eq!(back.registries.get("ghcr.io").unwrap().username, "alice");
    }

    /// Full set/get/overwrite/remove flow against a `FakeKeyring` —
    /// exercises the vault's state machine (opening, flushing) without
    /// needing a real keyring.
    #[test]
    fn secret_roundtrip_through_fake_keyring() {
        let vault = Vault::new_with(FakeKeyring::default(), HashMap::new());
        let name = "DATABASE_URL";

        vault.set_secret(name, "value-1").unwrap();
        assert_eq!(vault.get_secret(name).unwrap(), Some("value-1".to_string()));

        // Overwrite.
        vault.set_secret(name, "value-2").unwrap();
        assert_eq!(vault.get_secret(name).unwrap(), Some("value-2".to_string()));

        // Remove: first call returns true, second is a no-op.
        assert!(vault.remove_secret(name).unwrap());
        assert!(vault.get_secret(name).unwrap().is_none());
        assert!(!vault.remove_secret(name).unwrap(), "idempotent remove");
    }

    /// The vault must not contact the keyring until the caller asks
    /// for something. A read-only accessor with nothing to read still
    /// opens (to parse the blob), but constructing the `Vault` alone
    /// must stay silent — this is the whole point of lazy opening.
    #[test]
    fn construction_does_not_touch_keyring() {
        struct PanickingKeyring;
        impl Keyring for PanickingKeyring {
            fn load(&self) -> anyhow::Result<Option<String>> {
                panic!("load called during construction");
            }
            fn store(&self, _: &str) -> anyhow::Result<()> {
                panic!("store called during construction");
            }
        }
        // Construct and drop. No load/store must fire.
        let _ = Vault::new_with(PanickingKeyring, HashMap::new());
    }

    /// `FakeKeyring` shared across two `Vault` handles — each vault
    /// owns its `Keyring` box, so to simulate two processes talking to
    /// the same underlying storage we share state via `Arc`.
    struct SharedKeyring(Arc<FakeKeyring>);

    impl Keyring for SharedKeyring {
        fn load(&self) -> anyhow::Result<Option<String>> {
            self.0.load()
        }
        fn store(&self, data: &str) -> anyhow::Result<()> {
            self.0.store(data)
        }
    }

    /// A second `Vault` sharing the same backing keyring must see
    /// writes performed by the first. Confirms the flush path actually
    /// pushes JSON to the backend rather than just mutating in-memory.
    #[test]
    fn second_vault_sees_flushed_writes() {
        let shared = Arc::new(FakeKeyring::default());
        let a = Vault::new_with(SharedKeyring(shared.clone()), HashMap::new());
        a.set_secret("TOKEN", "abc123").unwrap();
        a.set_registry(
            "ghcr.io",
            &RegistryCreds {
                username: "alice".to_string(),
                password: "hunter2".to_string(),
            },
        )
        .unwrap();

        let b = Vault::new_with(SharedKeyring(shared), HashMap::new());
        assert_eq!(b.get_secret("TOKEN").unwrap(), Some("abc123".to_string()));
        let creds = b.get_registry("ghcr.io").unwrap().unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "hunter2");
    }

    /// Empty values and bad names must be rejected at the API, never
    /// reaching the keyring.
    #[test]
    fn set_secret_rejects_empty_and_bad_names() {
        let vault = Vault::new_with(FakeKeyring::default(), HashMap::new());
        assert!(vault.set_secret("FOO", "").is_err());
        assert!(vault.set_secret("lowercase", "x").is_err());
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// A template with no `${...}` must be returned verbatim and must
    /// not open the vault — opening the vault is what prompts for a
    /// keychain unlock on Linux, so the fast path keeps `airlock start`
    /// silent for projects whose env is fully literal.
    #[test]
    fn subst_literal_skips_vault_open() {
        struct PanickingKeyring;
        impl Keyring for PanickingKeyring {
            fn load(&self) -> anyhow::Result<Option<String>> {
                panic!("vault should not open for a literal template");
            }
            fn store(&self, _: &str) -> anyhow::Result<()> {
                panic!("vault should not write for a literal template");
            }
        }
        let vault = Vault::new_with(PanickingKeyring, HashMap::new());
        assert_eq!(vault.subst("plain-value").unwrap(), "plain-value");
        assert_eq!(vault.subst("").unwrap(), "");
    }

    /// Env-only lookup: no secret in vault, value comes from the
    /// snapshotted env map.
    #[test]
    fn subst_resolves_from_env() {
        let vault = Vault::new_with(FakeKeyring::default(), env(&[("HOME_DIR", "/home/alice")]));
        assert_eq!(
            vault.subst("prefix:${HOME_DIR}/suffix").unwrap(),
            "prefix:/home/alice/suffix"
        );
    }

    /// Secret-only lookup: no matching env entry, value comes from the
    /// vault.
    #[test]
    fn subst_resolves_from_vault() {
        let vault = Vault::new_with(FakeKeyring::default(), HashMap::new());
        vault.set_secret("DATABASE_URL", "postgres://db").unwrap();
        assert_eq!(
            vault.subst("url=${DATABASE_URL}").unwrap(),
            "url=postgres://db"
        );
    }

    /// Env must win when both sources define the same name. Host env
    /// is consulted first so routine templates (`${PATH}`, `${HOME}`)
    /// don't trigger a keyring unlock — that UX cost is worse than
    /// the footgun of a shell var shadowing a saved secret, which
    /// users can fix by renaming or unsetting.
    #[test]
    fn subst_env_wins_over_vault() {
        let vault = Vault::new_with(FakeKeyring::default(), env(&[("TOKEN", "from-env")]));
        vault.set_secret("TOKEN", "from-vault").unwrap();
        assert_eq!(vault.subst("${TOKEN}").unwrap(), "from-env");
    }

    /// An unknown variable has no value in either source — `subst`
    /// surfaces the underlying `subst` crate error as `anyhow::Error`.
    #[test]
    fn subst_missing_variable_errors() {
        let vault = Vault::new_with(FakeKeyring::default(), HashMap::new());
        assert!(vault.subst("${NOPE}").is_err());
    }

    /// One template can mix multiple references, drawing from both
    /// sources in a single pass.
    #[test]
    fn subst_mixes_vault_and_env_in_one_template() {
        let vault = Vault::new_with(FakeKeyring::default(), env(&[("USER", "alice")]));
        vault.set_secret("TOKEN", "s3cret").unwrap();
        assert_eq!(vault.subst("${USER}:${TOKEN}").unwrap(), "alice:s3cret");
    }
}
