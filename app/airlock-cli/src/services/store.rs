//! The token store of the network services: the database `services` of
//! the airlock database `~/.airlock/db/` ([`crate::db`]), shared by every
//! airlock process of the user. It holds the sign-ins (*grants*): the
//! real tokens a provider issued, the surrogates the sandbox got for
//! them, and the OAuth client and scopes of each sign-in.
//!
//! ## Layout
//!
//! Three keys per service:
//!
//! - `<service>.secrets`: all grants of the service in one JSON document
//!   ([`Secrets`]), sealed with ChaCha20-Poly1305 under a key from the
//!   vault. The AAD is the key name and the format version, so a sealed
//!   value cannot move to another key. A copied database is useless
//!   without the vault.
//! - `<service>.meta`: plain JSON for `airlock show` ([`list_grants`]):
//!   per grant id the account's email address, organization, scopes,
//!   client id, times and whether it has a refresh token. No token, no
//!   surrogate.
//! - `<service>.generation`: a `u64` (big-endian) that every write
//!   transaction increments.
//!
//! A write is one read-modify-write transaction that writes all three
//! keys ([`TokenStore::update`]). A reader reads the generation and
//! decrypts the secrets (and rebuilds its in-memory index surrogate →
//! grant and token, [`Snapshot`]) only when the generation changed since
//! its last read: a sign-out or refresh in another process is seen on the
//! next lookup, without a time-to-live. The files never map a surrogate
//! to a grant in plain text, and there is no HMAC lookup table.
//!
//! The databases `services.grants` and `services.lookups` of an earlier
//! layout are emptied when a process first uses the store: `heed` cannot
//! delete a named database, so their names stay, without records.
//!
//! ## Concurrency
//!
//! Every transaction runs whole on a blocking thread through
//! [`crate::db::Db`]: LMDB serializes the writers across processes, and a
//! transaction is short and never spans an `await` or network I/O.
//!
//! The agent refreshes its own tokens (see [`super::oauth`]); the proxy
//! only relays that refresh and stores its answer on the grant's
//! *current* record (re-read inside the write transaction, never the
//! value the caller last saw). Two refreshes of the same grant racing (two
//! sandboxes, or an agent that retries) each run their own upstream call
//! and then their own write; LMDB serializes the writes, so neither
//! corrupts the other's, but the one that commits second overwrites the
//! first (as two agents racing a refresh on a host would).
//!
//! ## Limits
//!
//! - A grant keeps at most [`MAX_API_KEYS`] created API keys; a new one
//!   drops the oldest (its surrogate stops working).
//! - At most [`API_KEY_CREATIONS`] API keys are created per grant in
//!   [`API_KEY_WINDOW_MS`] ([`TokenStore::take_api_key_slot`]); the times
//!   are in the store, so the limit holds across processes.
//! - An access surrogate a refresh re-mints keeps working until the expiry
//!   of its own real token, capped at [`MAX_PREVIOUS_ACCESS`]
//!   ([`Grant::previous_access`]): an agent (or another sandbox sharing the
//!   grant) that cached the old surrogate is not cut off right away.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit as _, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use heed::types::{Bytes, Str};
use heed::{Database, RoTxn, RwTxn};
use hmac::{Hmac, Mac as _};
use rand::TryRng as _;
use rand::rngs::SysRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::OnceCell;

use super::ServiceId;
use super::tokens::{Token, TokenKind};
use crate::db::Db;

/// The database of the store.
pub const DATABASE: &str = "services";

/// The databases of the earlier layout, emptied on first use.
pub const OLD_DATABASES: &[&str] = &["services.grants", "services.lookups"];

/// The version of the sealed format, in the AAD and the first byte.
const FORMAT_VERSION: u8 = 1;

/// The most API keys a grant keeps.
pub const MAX_API_KEYS: usize = 8;

/// The most API keys created with one grant in [`API_KEY_WINDOW_MS`].
pub const API_KEY_CREATIONS: usize = 3;

/// The window of [`API_KEY_CREATIONS`].
pub const API_KEY_WINDOW_MS: i64 = 60 * 60 * 1000;

/// The most earlier access surrogates [`Grant::previous_access`] keeps.
pub const MAX_PREVIOUS_ACCESS: usize = 4;

/// One sign-in, decrypted. No `Debug` with fields: it holds real tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct Grant {
    /// The key in [`Secrets::grants`].
    #[serde(skip)]
    pub id: String,
    /// The provider's id of the account: a new sign-in of the same
    /// account, client and scopes replaces this grant.
    pub account_id: String,
    /// The account's email address, for `airlock show`.
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    /// The OAuth client of the code exchange: refresh and revoke use it,
    /// never the one of the guest's request.
    pub client_id: String,
    /// The scopes the provider granted.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The real tokens and their surrogates; the first of each kind is the
    /// grant's main token of that kind.
    pub tokens: Vec<Token>,
    /// Access surrogates a refresh replaced, valid until their expiry: they
    /// stand for the current access token.
    #[serde(default)]
    pub previous_access: Vec<PreviousAccess>,
    /// API keys created with the grant, each with its surrogate.
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
    /// Unix ms of the API keys created in the last [`API_KEY_WINDOW_MS`].
    #[serde(default)]
    pub api_key_creations: Vec<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Names the grant, never its secrets.
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Grant {
    /// The main token of `kind`.
    pub fn token(&self, kind: TokenKind) -> Option<&Token> {
        self.tokens.iter().find(|t| t.kind == kind)
    }

    /// The real main token of `kind`.
    pub fn real(&self, kind: TokenKind) -> Option<&str> {
        self.token(kind).map(|t| t.real.as_str())
    }

    /// Keep `surrogate` (replaced by a new one) valid until `expires_at`:
    /// drop entries past their expiry, then the oldest when
    /// [`MAX_PREVIOUS_ACCESS`] is full.
    pub fn keep_previous_access(&mut self, surrogate: String, expires_at: i64) {
        let now = now_ms();
        self.previous_access.retain(|p| p.expires_at > now);
        if self.previous_access.len() >= MAX_PREVIOUS_ACCESS {
            self.previous_access.remove(0);
        }
        self.previous_access.push(PreviousAccess {
            surrogate,
            expires_at,
        });
    }

    /// What a new sign-in replaces: same account, client and scopes.
    fn same_sign_in(&self, other: &Grant) -> bool {
        let sorted = |g: &Grant| {
            let mut s = g.scopes.clone();
            s.sort();
            s
        };
        self.account_id == other.account_id
            && self.client_id == other.client_id
            && sorted(self) == sorted(other)
    }

    #[cfg(test)]
    pub fn for_tests(tokens: Vec<Token>) -> Self {
        Self {
            id: String::new(),
            account_id: "acct".into(),
            account: None,
            organization: None,
            client_id: "client".into(),
            scopes: vec![],
            tokens,
            previous_access: vec![],
            api_keys: vec![],
            api_key_creations: vec![],
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// One access surrogate a refresh replaced. See
/// [`Grant::previous_access`].
#[derive(Clone, Serialize, Deserialize)]
pub struct PreviousAccess {
    pub surrogate: String,
    /// Unix ms.
    pub expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub real: String,
    pub surrogate: String,
}

/// A new grant from a code exchange; the store gives it its id and times.
pub struct NewGrant {
    pub account_id: String,
    pub account: Option<String>,
    pub organization: Option<String>,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub tokens: Vec<Token>,
}

/// The sealed document of `<service>.secrets`.
#[derive(Default, Serialize, Deserialize)]
pub struct Secrets {
    pub grants: BTreeMap<String, Grant>,
}

/// What `<service>.meta` holds of a grant: no secrets.
#[derive(Serialize, Deserialize)]
struct GrantMeta {
    account: Option<String>,
    organization: Option<String>,
    scopes: Vec<String>,
    client_id: String,
    created_at: i64,
    updated_at: i64,
    has_refresh: bool,
}

impl GrantMeta {
    fn of(grant: &Grant) -> Self {
        Self {
            account: grant.account.clone(),
            organization: grant.organization.clone(),
            scopes: grant.scopes.clone(),
            client_id: grant.client_id.clone(),
            created_at: grant.created_at,
            updated_at: grant.updated_at,
            has_refresh: grant.token(TokenKind::Refresh).is_some(),
        }
    }
}

/// What `airlock show` lists of a grant: no secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSummary {
    pub service: String,
    pub account: Option<String>,
    pub scopes: Vec<String>,
    /// Unix ms of the sign-in.
    pub created_at: i64,
}

/// What a surrogate stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// `tokens[i]`.
    Token(usize),
    /// `previous_access[i]`: the current access token, until its expiry.
    PreviousAccess(usize),
    /// `api_keys[i]`.
    ApiKey(usize),
}

/// A surrogate the store knows.
#[derive(Clone)]
pub struct Resolved {
    pub grant_id: String,
    /// The kind of token it stands for.
    pub kind: TokenKind,
    /// The real value to send in its place.
    pub real: String,
}

/// The grants of one service as of one generation, with the index of
/// their surrogates.
pub struct Snapshot {
    generation: u64,
    grants: HashMap<String, Grant>,
    index: HashMap<String, (String, Slot)>,
}

impl Snapshot {
    fn build(generation: u64, secrets: Secrets) -> Self {
        let mut grants = HashMap::new();
        let mut index = HashMap::new();
        for (id, mut grant) in secrets.grants {
            grant.id.clone_from(&id);
            for (i, t) in grant.tokens.iter().enumerate() {
                index.insert(t.surrogate.clone(), (id.clone(), Slot::Token(i)));
            }
            for (i, p) in grant.previous_access.iter().enumerate() {
                index
                    .entry(p.surrogate.clone())
                    .or_insert((id.clone(), Slot::PreviousAccess(i)));
            }
            for (i, k) in grant.api_keys.iter().enumerate() {
                index.insert(k.surrogate.clone(), (id.clone(), Slot::ApiKey(i)));
            }
            grants.insert(id, grant);
        }
        Self {
            generation,
            grants,
            index,
        }
    }

    /// The token `surrogate` stands for. A previous access surrogate
    /// stands for the current access token until its own expiry; after
    /// that it is unknown.
    pub fn resolve(&self, surrogate: &str) -> Option<Resolved> {
        let (id, slot) = self.index.get(surrogate)?;
        let grant = self.grants.get(id)?;
        let (kind, real) = match *slot {
            Slot::Token(i) => {
                let t = grant.tokens.get(i)?;
                (t.kind, t.real.clone())
            }
            Slot::PreviousAccess(i) => {
                if grant.previous_access.get(i)?.expires_at <= now_ms() {
                    return None;
                }
                (
                    TokenKind::Access,
                    grant.real(TokenKind::Access)?.to_string(),
                )
            }
            Slot::ApiKey(i) => (TokenKind::ApiKey, grant.api_keys.get(i)?.real.clone()),
        };
        Some(Resolved {
            grant_id: id.clone(),
            kind,
            real,
        })
    }

    /// Every real token and API key of the service.
    pub fn reals(&self) -> impl Iterator<Item = &str> {
        self.grants.values().flat_map(|g| {
            g.tokens
                .iter()
                .map(|t| t.real.as_str())
                .chain(g.api_keys.iter().map(|k| k.real.as_str()))
        })
    }

    /// The grant of the surrogate `surrogate` of one of `kinds`.
    pub fn grant_of(&self, surrogate: &str, kinds: &[TokenKind]) -> Option<&Grant> {
        let resolved = self.resolve(surrogate)?;
        kinds
            .contains(&resolved.kind)
            .then(|| self.grants.get(&resolved.grant_id))
            .flatten()
    }
}

/// The store key from the vault, as the encryption key.
#[derive(Clone)]
struct Keys {
    encryption: [u8; 32],
}

impl Keys {
    fn derive(master: &[u8; 32]) -> Self {
        Self {
            encryption: hmac_sha256(master, &[b"airlock services store: encryption"]),
        }
    }

    /// `[version] nonce ciphertext` of `plain` under the key name `key`.
    fn seal(&self, key: &str, plain: &[u8]) -> anyhow::Result<Vec<u8>> {
        let nonce = random_bytes::<12>()?;
        let ciphertext = ChaCha20Poly1305::new(<&Key>::from(&self.encryption))
            .encrypt(
                <&Nonce>::from(&nonce),
                Payload {
                    msg: plain,
                    aad: &aad(key),
                },
            )
            .map_err(|_| anyhow!("encrypt {key}"))?;
        let mut out = vec![FORMAT_VERSION];
        out.extend_from_slice(&nonce);
        out.extend(ciphertext);
        Ok(out)
    }

    fn open(&self, key: &str, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        let Some((&version, rest)) = sealed.split_first() else {
            bail!("{key} is empty");
        };
        if version != FORMAT_VERSION {
            bail!("{key} has the unknown format version {version}");
        }
        if rest.len() < 12 {
            bail!("{key} is too short");
        }
        let (nonce, ciphertext) = rest.split_at(12);
        let nonce = <[u8; 12]>::try_from(nonce).expect("12 bytes");
        ChaCha20Poly1305::new(<&Key>::from(&self.encryption))
            .decrypt(
                <&Nonce>::from(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad(key),
                },
            )
            .map_err(|_| anyhow!("decrypt {key}: wrong key or corrupt data"))
    }
}

/// The AAD of the sealed value of the key `key`.
fn aad(key: &str) -> Vec<u8> {
    format!("airlock-services\0{key}\0v{FORMAT_VERSION}").into_bytes()
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key)
        .expect("HMAC takes keys of any length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// `N` bytes from the operating system's CSPRNG.
pub fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow!("random bytes: {e}"))?;
    Ok(bytes)
}

/// Unix time in ms.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The name of the key `part` of `service`.
fn key_name(service: ServiceId, part: &str) -> String {
    format!("{}.{part}", service.name())
}

type Table = Database<Str, Bytes>;

fn read_generation(table: Table, txn: &RoTxn, service: ServiceId) -> anyhow::Result<u64> {
    let Some(bytes) = table.get(txn, &key_name(service, "generation"))? else {
        return Ok(0);
    };
    let bytes = <[u8; 8]>::try_from(bytes).map_err(|_| anyhow!("corrupt store generation"))?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_secrets(
    table: Table,
    txn: &RoTxn,
    keys: &Keys,
    service: ServiceId,
) -> anyhow::Result<Secrets> {
    let key = key_name(service, "secrets");
    let Some(sealed) = table.get(txn, &key)? else {
        return Ok(Secrets::default());
    };
    let plain = keys.open(&key, sealed)?;
    serde_json::from_slice(&plain).with_context(|| format!("parse {key}"))
}

/// Write all three keys of `service`; returns the new generation.
fn write_all(
    table: Table,
    txn: &mut RwTxn,
    keys: &Keys,
    service: ServiceId,
    generation: u64,
    secrets: &Secrets,
) -> anyhow::Result<u64> {
    let key = key_name(service, "secrets");
    let plain = serde_json::to_vec(secrets).context("serialize the grants")?;
    table.put(txn, &key, &keys.seal(&key, &plain)?)?;
    let meta: BTreeMap<&String, GrantMeta> = secrets
        .grants
        .iter()
        .map(|(id, g)| (id, GrantMeta::of(g)))
        .collect();
    table.put(txn, &key_name(service, "meta"), &serde_json::to_vec(&meta)?)?;
    let next = generation.wrapping_add(1);
    table.put(txn, &key_name(service, "generation"), &next.to_be_bytes())?;
    Ok(next)
}

/// The token store of one process (one per sandbox session) in the
/// airlock database. Opens its database on first use.
pub struct TokenStore {
    db: Db,
    keys: Keys,
    table: OnceCell<Table>,
    /// The last snapshot this process read of each service.
    cache: parking_lot::Mutex<HashMap<ServiceId, Arc<Snapshot>>>,
}

impl TokenStore {
    /// A store in the database `db` with the vault's store key. Touches
    /// nothing yet.
    pub fn new(db: Db, master_key: &[u8; 32]) -> Self {
        Self {
            db,
            keys: Keys::derive(master_key),
            table: OnceCell::new(),
            cache: parking_lot::Mutex::default(),
        }
    }

    /// The database (created on first use; the old databases emptied).
    async fn table(&self) -> anyhow::Result<Table> {
        self.table
            .get_or_try_init(|| async {
                for old in OLD_DATABASES {
                    if self.db.empty_database(old).await? {
                        tracing::debug!("emptied the old token store database {old}");
                    }
                }
                self.db.database(DATABASE).await
            })
            .await
            .copied()
    }

    /// The grants of `service` as they are now: the cached snapshot when
    /// the generation did not change, else decrypted again.
    pub async fn snapshot(&self, service: ServiceId) -> anyhow::Result<Arc<Snapshot>> {
        let table = self.table().await?;
        let cached = self.cache.lock().get(&service).cloned();
        let cached_generation = cached.as_ref().map(|s| s.generation);
        let keys = self.keys.clone();
        let loaded = self
            .db
            .read(move |txn| {
                let generation = read_generation(table, txn, service)?;
                if Some(generation) == cached_generation {
                    return Ok(None);
                }
                Ok(Some((
                    generation,
                    read_secrets(table, txn, &keys, service)?,
                )))
            })
            .await?;
        match (loaded, cached) {
            (None, Some(cached)) => Ok(cached),
            (Some((generation, secrets)), _) => {
                Ok(self.cache_snapshot(service, generation, secrets))
            }
            (None, None) => unreachable!("a read without a cached generation loads"),
        }
    }

    fn cache_snapshot(
        &self,
        service: ServiceId,
        generation: u64,
        secrets: Secrets,
    ) -> Arc<Snapshot> {
        let snapshot = Arc::new(Snapshot::build(generation, secrets));
        self.cache.lock().insert(service, snapshot.clone());
        snapshot
    }

    /// Change the grants of `service` in one write transaction: `change`
    /// gets them as they stand now (never a cached copy), and all three
    /// keys are written with the next generation.
    pub async fn update<T, F>(&self, service: ServiceId, change: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Secrets) -> anyhow::Result<T> + Send + 'static,
    {
        let table = self.table().await?;
        let keys = self.keys.clone();
        let (value, generation, secrets) = self
            .db
            .write(move |txn| {
                let generation = read_generation(table, txn, service)?;
                let mut secrets = read_secrets(table, txn, &keys, service)?;
                let value = change(&mut secrets)?;
                for (id, grant) in &mut secrets.grants {
                    grant.id.clone_from(id);
                }
                let next = write_all(table, txn, &keys, service, generation, &secrets)?;
                Ok((value, next, secrets))
            })
            .await?;
        self.cache_snapshot(service, generation, secrets);
        Ok(value)
    }

    /// The token `surrogate` of `service` stands for, if the store knows
    /// it.
    #[cfg(test)]
    pub async fn resolve(
        &self,
        service: ServiceId,
        surrogate: &str,
    ) -> anyhow::Result<Option<Resolved>> {
        Ok(self.snapshot(service).await?.resolve(surrogate))
    }

    /// The grant of the surrogate `surrogate` of one of `kinds`, as it is
    /// now.
    pub async fn grant_of(
        &self,
        service: ServiceId,
        surrogate: &str,
        kinds: &[TokenKind],
    ) -> anyhow::Result<Option<Grant>> {
        Ok(self
            .snapshot(service)
            .await?
            .grant_of(surrogate, kinds)
            .cloned())
    }

    /// Store a new grant. Older grants of the same account, client and
    /// scopes are deleted in the same transaction: the new sign-in
    /// replaces them. Returns the new grant and the replaced ones (to
    /// revoke upstream).
    pub async fn insert_grant(
        &self,
        service: ServiceId,
        new: NewGrant,
    ) -> anyhow::Result<(Grant, Vec<Grant>)> {
        let id = hex::encode(random_bytes::<16>()?);
        let now = now_ms();
        let grant = Grant {
            id: id.clone(),
            account_id: new.account_id,
            account: new.account,
            organization: new.organization,
            client_id: new.client_id,
            scopes: new.scopes,
            tokens: new.tokens,
            previous_access: vec![],
            api_keys: vec![],
            api_key_creations: vec![],
            created_at: now,
            updated_at: now,
        };
        let stored = grant.clone();
        let replaced = self
            .update(service, move |secrets| {
                let old: Vec<String> = secrets
                    .grants
                    .iter()
                    .filter(|(_, g)| g.same_sign_in(&stored))
                    .map(|(id, _)| id.clone())
                    .collect();
                let replaced: Vec<Grant> = old
                    .iter()
                    .filter_map(|id| {
                        let mut g = secrets.grants.remove(id)?;
                        g.id.clone_from(id);
                        Some(g)
                    })
                    .collect();
                secrets.grants.insert(id, stored);
                Ok(replaced)
            })
            .await?;
        Ok((grant, replaced))
    }

    /// Change the grant `id` of `service` as it stands now. `Ok(None)`:
    /// the grant is gone (a sign-out meanwhile); nothing is changed.
    pub async fn update_grant<T, F>(
        &self,
        service: ServiceId,
        id: &str,
        change: F,
    ) -> anyhow::Result<Option<(Grant, T)>>
    where
        T: Send + 'static,
        F: FnOnce(&mut Grant) -> anyhow::Result<T> + Send + 'static,
    {
        let id = id.to_string();
        self.update(service, move |secrets| {
            let Some(grant) = secrets.grants.get_mut(&id) else {
                return Ok(None);
            };
            grant.id.clone_from(&id);
            let value = change(grant)?;
            grant.updated_at = now_ms();
            Ok(Some((grant.clone(), value)))
        })
        .await
    }

    /// Record API keys created with the grant (and their surrogates). The
    /// oldest keys go beyond [`MAX_API_KEYS`].
    pub async fn add_api_keys(
        &self,
        service: ServiceId,
        grant_id: &str,
        keys: Vec<ApiKey>,
    ) -> anyhow::Result<()> {
        let done = self
            .update_grant(service, grant_id, move |grant| {
                grant.api_keys.extend(keys);
                let excess = grant.api_keys.len().saturating_sub(MAX_API_KEYS);
                grant.api_keys.drain(..excess);
                Ok(())
            })
            .await?;
        if done.is_none() {
            bail!("the sign-in was removed");
        }
        Ok(())
    }

    /// Count one API key creation with grant `grant_id`: `false` (and
    /// nothing counted) when the grant already created
    /// [`API_KEY_CREATIONS`] keys in the last [`API_KEY_WINDOW_MS`], or is
    /// gone.
    pub async fn take_api_key_slot(
        &self,
        service: ServiceId,
        grant_id: &str,
    ) -> anyhow::Result<bool> {
        let taken = self
            .update_grant(service, grant_id, |grant| {
                let now = now_ms();
                grant
                    .api_key_creations
                    .retain(|t| now - t < API_KEY_WINDOW_MS && *t <= now);
                if grant.api_key_creations.len() >= API_KEY_CREATIONS {
                    return Ok(false);
                }
                grant.api_key_creations.push(now);
                Ok(true)
            })
            .await?;
        Ok(taken.is_some_and(|(_, taken)| taken))
    }

    /// Delete the grant `grant_id` (a sign-out). Deleting a grant that is
    /// gone is no error.
    pub async fn delete_grant(&self, service: ServiceId, grant_id: &str) -> anyhow::Result<()> {
        let id = grant_id.to_string();
        self.update(service, move |secrets| {
            secrets.grants.remove(&id);
            Ok(())
        })
        .await
    }
}

/// The grants stored in `db`, without the key: for `airlock show`. Reads
/// only `<service>.meta`.
pub async fn list_grants(db: &Db) -> anyhow::Result<Vec<GrantSummary>> {
    let table: Table = db.database(DATABASE).await?;
    db.read(move |txn| {
        let mut out = Vec::new();
        for service in ServiceId::ALL {
            let Some(bytes) = table.get(txn, &key_name(service, "meta"))? else {
                continue;
            };
            let meta: BTreeMap<String, GrantMeta> =
                serde_json::from_slice(bytes).context("parse the sign-ins")?;
            out.extend(meta.into_values().map(|m| GrantSummary {
                service: service.name().to_string(),
                account: m.account,
                scopes: m.scopes,
                created_at: m.created_at,
            }));
        }
        out.sort_by(|a, b| (&a.service, a.created_at).cmp(&(&b.service, b.created_at)));
        Ok(out)
    })
    .await
}

#[cfg(test)]
mod tests;
