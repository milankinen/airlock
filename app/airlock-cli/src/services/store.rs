//! The token store of the network services.
//!
//! Keeps the sign-ins (*grants*) encrypted in the airlock database. All
//! airlock processes of the user share the store. A grant holds the real
//! tokens that a provider issued, the surrogates that the sandbox got for
//! them, and the OAuth client and scopes of the sign-in.
//!
//! The store also lists the grants for `airlock show`, and limits how many
//! API keys and replaced surrogates a grant can have. A copied database is
//! useless without the vault.

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

/// Name of the database of the store.
pub const DATABASE: &str = "services";

/// Databases of an earlier layout. The store empties them on first use.
/// `heed` cannot delete a named database, so their names stay, without
/// records.
pub const OLD_DATABASES: &[&str] = &["services.grants", "services.lookups"];

/// Version of the sealed format, in the AAD and in the first byte.
const FORMAT_VERSION: u8 = 1;

/// Maximum number of API keys that a grant keeps. A new key removes the
/// oldest key, and the surrogate of that key stops working.
pub const MAX_API_KEYS: usize = 8;

/// Maximum number of API keys that one grant can create in
/// [`API_KEY_WINDOW_MS`]. The creation times are in the store, so the
/// limit applies across processes.
pub const API_KEY_CREATIONS: usize = 3;

/// Time window of [`API_KEY_CREATIONS`], in ms.
pub const API_KEY_WINDOW_MS: i64 = 60 * 60 * 1000;

/// Maximum number of replaced access surrogates in
/// [`Grant::previous_access`].
pub const MAX_PREVIOUS_ACCESS: usize = 4;

/// One sign-in, decrypted. Its `Debug` shows no fields, because it holds
/// real tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct Grant {
    /// The key in [`Secrets::grants`].
    #[serde(skip)]
    pub id: String,
    /// The provider's account id. A new sign-in of the same account,
    /// client and scopes replaces this grant.
    pub account_id: String,
    /// The account's email address, for `airlock show`.
    #[serde(default)]
    pub account: Option<String>,
    /// The organization name, if the provider named one.
    #[serde(default)]
    pub organization: Option<String>,
    /// The OAuth client of the code exchange. Refresh and revoke use it,
    /// never the client of the guest's request.
    pub client_id: String,
    /// The scopes the provider granted.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// The real tokens and their surrogates. The first token of each kind
    /// is the grant's main token of that kind.
    pub tokens: Vec<Token>,
    /// Access surrogates that a refresh replaced. Until its expiry, each
    /// one stands for the current access token. Thus an agent (or another
    /// sandbox that shares the grant) with the old surrogate in its cache
    /// can continue to work. The expiry is that of the old real token, or
    /// one hour after the refresh if that expiry is not known.
    #[serde(default)]
    pub previous_access: Vec<PreviousAccess>,
    /// API keys that the grant created, each with its surrogate.
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
    /// Creation times (Unix ms) of the API keys created in the last
    /// [`API_KEY_WINDOW_MS`].
    #[serde(default)]
    pub api_key_creations: Vec<i64>,
    /// Time of the sign-in (Unix ms).
    pub created_at: i64,
    /// Time of the last change (Unix ms).
    pub updated_at: i64,
}

/// Shows the grant id, never its secrets.
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Grant {
    /// Get the main token of `kind`.
    pub fn token(&self, kind: TokenKind) -> Option<&Token> {
        self.tokens.iter().find(|t| t.kind == kind)
    }

    /// Get the real value of the main token of `kind`.
    pub fn real(&self, kind: TokenKind) -> Option<&str> {
        self.token(kind).map(|t| t.real.as_str())
    }

    /// Keep the replaced access surrogate `surrogate` valid until
    /// `expires_at`.
    ///
    /// First removes the expired entries. Then removes the oldest entry if
    /// there are [`MAX_PREVIOUS_ACCESS`] entries.
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

    /// Whether a new sign-in `other` replaces this grant: same account,
    /// client and scopes.
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

    /// A grant with `tokens`, account `acct` and client `client`.
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

/// One access surrogate that a refresh replaced. See
/// [`Grant::previous_access`].
#[derive(Clone, Serialize, Deserialize)]
pub struct PreviousAccess {
    /// The replaced surrogate.
    pub surrogate: String,
    /// When the surrogate stops working (Unix ms).
    pub expires_at: i64,
}

/// An API key that a grant created.
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// The real key.
    pub real: String,
    /// The surrogate that the sandbox gets.
    pub surrogate: String,
}

/// A new grant from a code exchange. The store gives it its id and times.
/// See [`Grant`] for the fields.
pub struct NewGrant {
    /// The provider's account id.
    pub account_id: String,
    /// The account's email address.
    pub account: Option<String>,
    /// The organization name.
    pub organization: Option<String>,
    /// The OAuth client of the code exchange.
    pub client_id: String,
    /// The scopes that the provider granted.
    pub scopes: Vec<String>,
    /// The real tokens and their surrogates.
    pub tokens: Vec<Token>,
}

/// The sealed document of the `<service>.secrets` key: all grants of the
/// service.
///
/// Each service has three keys in the database:
///  * `<service>.secrets`: this document, sealed with ChaCha20-Poly1305
///    under a key from the vault. The AAD is the key name and the format
///    version, so a sealed value cannot move to another key.
///  * `<service>.meta`: plain JSON for `airlock show` ([`list_grants`]).
///    No token, no surrogate.
///  * `<service>.generation`: a `u64` (big-endian) that every write
///    transaction increments.
///
/// The files never map a surrogate to a grant in plain text, and there is
/// no HMAC lookup table.
#[derive(Default, Serialize, Deserialize)]
pub struct Secrets {
    /// The grants, by grant id.
    pub grants: BTreeMap<String, Grant>,
}

/// The data of a grant in `<service>.meta`: no secrets. Per grant id: the
/// account's email address, organization, scopes, client id, times and
/// whether the grant has a refresh token.
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

/// The data of a grant that `airlock show` lists: no secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantSummary {
    /// The service name.
    pub service: String,
    /// The account's email address.
    pub account: Option<String>,
    /// The scopes that the provider granted.
    pub scopes: Vec<String>,
    /// Unix ms of the sign-in.
    pub created_at: i64,
}

/// The token that a surrogate stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// `tokens[i]`.
    Token(usize),
    /// `previous_access[i]`: stands for the current access token, until
    /// its expiry.
    PreviousAccess(usize),
    /// `api_keys[i]`.
    ApiKey(usize),
}

/// A surrogate that the store knows, and its real value.
#[derive(Clone)]
pub struct Resolved {
    /// The id of the grant of the surrogate.
    pub grant_id: String,
    /// The kind of token that the surrogate stands for.
    pub kind: TokenKind,
    /// The real value to send in place of the surrogate.
    pub real: String,
}

/// The grants of one service at one generation, with an in-memory index
/// from surrogate to grant and token.
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

    /// Find the token that `surrogate` stands for.
    ///
    /// A replaced access surrogate stands for the current access token
    /// until its own expiry. After that, it is unknown.
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

    /// Get every real token and API key of the service.
    pub fn reals(&self) -> impl Iterator<Item = &str> {
        self.grants.values().flat_map(|g| {
            g.tokens
                .iter()
                .map(|t| t.real.as_str())
                .chain(g.api_keys.iter().map(|k| k.real.as_str()))
        })
    }

    /// Find the grant of `surrogate`, if the surrogate is of one of
    /// `kinds`.
    pub fn grant_of(&self, surrogate: &str, kinds: &[TokenKind]) -> Option<&Grant> {
        let resolved = self.resolve(surrogate)?;
        kinds
            .contains(&resolved.kind)
            .then(|| self.grants.get(&resolved.grant_id))
            .flatten()
    }
}

/// The encryption key, derived from the store key of the vault.
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

    /// Seal `plain` for the database key `key`.
    /// Returns:
    ///   `[version] nonce ciphertext`.
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

/// Make the AAD of the sealed value of the database key `key`.
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

/// Get `N` bytes from the CSPRNG of the operating system.
pub fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow!("random bytes: {e}"))?;
    Ok(bytes)
}

/// Get the current Unix time in ms.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Make the database key name of `part` of `service`.
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

/// Write all three keys of `service`. Returns the new generation.
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
///
/// A write is one read-modify-write transaction that writes all three
/// keys of a service ([`Self::update`]). A reader reads the generation.
/// It decrypts the secrets and makes its [`Snapshot`] again only if the
/// generation changed since its last read. Thus a sign-out or refresh in
/// another process is visible at the next lookup, without a time to live.
///
/// Every transaction runs fully on a blocking thread through
/// [`crate::db::Db`]. LMDB serializes the writers across processes. A
/// transaction is short and never includes an `await` or network I/O.
pub struct TokenStore {
    db: Db,
    keys: Keys,
    table: OnceCell<Table>,
    /// The last snapshot of each service that this process read.
    cache: parking_lot::Mutex<HashMap<ServiceId, Arc<Snapshot>>>,
}

impl TokenStore {
    /// Make a store in the database `db` with the store key `master_key`
    /// of the vault. Does not access the database yet.
    pub fn new(db: Db, master_key: &[u8; 32]) -> Self {
        Self {
            db,
            keys: Keys::derive(master_key),
            table: OnceCell::new(),
            cache: parking_lot::Mutex::default(),
        }
    }

    /// Get the database. On first use, creates it and empties the old
    /// databases.
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

    /// Get the current grants of `service`. Returns the cached snapshot if
    /// the generation did not change, else decrypts the grants again.
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

    /// Change the grants of `service` in one write transaction.
    ///
    /// `change` gets the current grants, read again in the transaction
    /// (never a cached copy). Then all three keys are written with the next
    /// generation.
    ///
    /// Two refreshes of the same grant that race (two sandboxes, or an
    /// agent that tries again) each do their own upstream call and their
    /// own write. LMDB serializes the writes, so neither write corrupts the
    /// other. But the second commit overwrites the first, as with two
    /// agents that race a refresh on a host.
    /// Args:
    ///  - `service`: The service of the grants
    ///  - `change`: Changes the grants and returns a value.
    ///
    /// Returns:
    ///   The value from `change`.
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

    /// Find the token that `surrogate` of `service` stands for, if the
    /// store knows it.
    #[cfg(test)]
    pub async fn resolve(
        &self,
        service: ServiceId,
        surrogate: &str,
    ) -> anyhow::Result<Option<Resolved>> {
        Ok(self.snapshot(service).await?.resolve(surrogate))
    }

    /// Find the current grant of `surrogate`, if the surrogate is of one
    /// of `kinds`.
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

    /// Store a new grant.
    ///
    /// Deletes the older grants of the same account, client and scopes in
    /// the same transaction, because the new sign-in replaces them.
    /// Returns:
    ///   The new grant, and the replaced grants (to revoke upstream).
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

    /// Change the current grant `id` of `service` with `change`.
    /// Returns:
    ///   The changed grant and the value from `change`. `Ok(None)` if the
    ///   grant does not exist (for example, after a sign-out). Then
    ///   nothing changes.
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

    /// Record API keys (and their surrogates) that the grant created.
    /// Removes the oldest keys above [`MAX_API_KEYS`].
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

    /// Count one API key creation for the grant `grant_id`.
    /// Returns:
    ///   `false` (and counts nothing) if the grant already created
    ///   [`API_KEY_CREATIONS`] keys in the last [`API_KEY_WINDOW_MS`], or
    ///   if the grant does not exist.
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

    /// Delete the grant `grant_id` (a sign-out). If the grant does not
    /// exist, this is not an error.
    pub async fn delete_grant(&self, service: ServiceId, grant_id: &str) -> anyhow::Result<()> {
        let id = grant_id.to_string();
        self.update(service, move |secrets| {
            secrets.grants.remove(&id);
            Ok(())
        })
        .await
    }
}

/// List the grants stored in `db`, for `airlock show`. Reads only the
/// `<service>.meta` keys, so it does not need the store key.
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
