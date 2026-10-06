//! The token store of the network services: the databases
//! `services.grants` and `services.lookups` of the airlock database
//! `~/.airlock/db/` ([`crate::db`]), shared by every airlock process of
//! the user. A record of `services.grants` is one sign-in (a *grant*):
//! the real tokens the provider issued and the surrogates the sandbox got
//! for them.
//!
//! ## Secrets at rest
//!
//! The secret part of a grant is one JSON document ([`GrantSecrets`]),
//! encrypted with ChaCha20-Poly1305 under a key from the vault (AAD: grant
//! id and service), so a copied environment is useless without the vault.
//! Surrogates are found through the `services.lookups` database, keyed by
//! HMAC-SHA256 of the surrogate: the files never map a surrogate to a
//! grant in plain text. The plain fields of a record carry only what
//! `airlock show` lists without the key: service, account label, scopes
//! and timestamps.
//!
//! ## Concurrency
//!
//! Every transaction runs whole on a blocking thread through
//! [`crate::db::Db`]: LMDB serializes the writers across processes, and a
//! transaction is short and never spans an `await` or network I/O.
//!
//! The agent refreshes its own tokens (see [`super::oauth`]); the proxy
//! only relays that refresh and stores its answer. [`TokenStore::replace_tokens`]
//! writes the new tokens in one transaction on the grant's *current*
//! record (re-read inside the transaction, never the value the caller
//! last saw): a `version + 1` and the lookups of any surrogate the
//! refresh re-minted follow in the same write. Two refreshes of the same
//! grant racing (two sandboxes, or an agent that retries) each run their
//! own upstream call and then their own write transaction; LMDB serializes
//! the writes, so neither corrupts the other's, but the one that commits
//! second simply overwrites the first (as two agents racing a refresh on
//! a host would).
//!
//! ## Limits
//!
//! - A grant keeps at most [`MAX_API_KEYS`] created API keys; a new one
//!   drops the oldest (its surrogate stops working).
//! - At most [`API_KEY_CREATIONS`] API keys are created per grant in
//!   [`API_KEY_WINDOW_MS`] ([`TokenStore::take_api_key_slot`]); the times are
//!   in the plain part of the record, so the limit holds across processes.
//! - An access surrogate a refresh re-mints ([`super::oauth::Provider::apply_refresh`])
//!   keeps working until its own expiry, capped at
//!   [`MAX_PREVIOUS_ACCESS`] ([`Surrogates::previous_access`]): an agent
//!   (or another sandbox sharing the grant) that cached the old surrogate
//!   is not cut off right away.
//!
//! ## Cache
//!
//! Each process caches the grants it decrypted. A cached grant is used
//! for at most [`CACHE_TTL`] before its lookup is read again from the
//! file: a sign-out in another process (the grant deleted), or a refresh
//! that already expired the cached access token, stops this process from
//! serving the cached copy within that time.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
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
use crate::db::Db;

/// The database of the grants: grant id → [`GrantRecord`] (JSON). A
/// grant takes a few KiB.
const GRANTS: &str = "services.grants";

/// The database of the lookups: HMAC of a surrogate → [`LookupRecord`]
/// (JSON).
const LOOKUPS: &str = "services.lookups";

/// The most API keys a grant keeps.
pub const MAX_API_KEYS: usize = 8;

/// The most API keys created with one grant in [`API_KEY_WINDOW_MS`].
pub const API_KEY_CREATIONS: usize = 3;

/// The window of [`API_KEY_CREATIONS`].
pub const API_KEY_WINDOW_MS: i64 = 60 * 60 * 1000;

/// The most earlier access surrogates [`Surrogates::previous_access`]
/// keeps.
pub const MAX_PREVIOUS_ACCESS: usize = 4;

/// How long a process uses a cached grant before it reads the store again.
const CACHE_TTL: Duration = Duration::from_secs(10);

/// The secret part of a grant, encrypted at rest. No `Debug`: it holds
/// real tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct GrantSecrets {
    pub access_token: String,
    /// When the real access token expires (Unix ms).
    pub access_expires_at: i64,
    /// `None` when the provider issued none (a long-lived token): the
    /// grant then ends with its access token.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// The provider's ID token (OpenID Connect), if it issued one.
    #[serde(default)]
    pub id_token: Option<String>,
    /// The scopes the grant was issued with; a refresh asks for them again.
    #[serde(default)]
    pub scopes: Vec<String>,
    pub surrogates: Surrogates,
    /// API keys created with the grant, each with its surrogate.
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
}

impl GrantSecrets {
    /// The expiry (Unix ms) of `surrogate` as a *previous* access
    /// surrogate of these secrets ([`Surrogates::previous_access`]), each
    /// kept only until its own `exp`. `None` when `surrogate` is the
    /// current access surrogate ([`Self::access_expires_at`]) — the
    /// current one has no deadline of its own, it resolves for as long as
    /// the grant exists — or not found at all, which should not happen,
    /// since the lookup table only points here for a surrogate these
    /// secrets issued.
    fn previous_access_expires_at(&self, surrogate: &str) -> Option<i64> {
        self.surrogates
            .previous_access
            .iter()
            .find(|p| p.surrogate == surrogate)
            .map(|p| p.expires_at)
    }
}

/// What the sandbox holds instead of the real tokens.
#[derive(Clone, Serialize, Deserialize)]
pub struct Surrogates {
    pub access: String,
    /// Access surrogates a refresh re-minted (see
    /// [`super::oauth::Provider::apply_refresh`]), kept working until their own `exp`
    /// (Unix ms), capped at [`MAX_PREVIOUS_ACCESS`]: an agent (or another
    /// sandbox sharing the grant) that cached one of them keeps working
    /// across the refresh.
    #[serde(default)]
    pub previous_access: Vec<PreviousAccess>,
    #[serde(default)]
    pub refresh: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

impl Surrogates {
    /// Keep `surrogate` (about to be replaced by a freshly re-minted one)
    /// working until `expires_at`: drop entries already past their own
    /// expiry, then the oldest if [`MAX_PREVIOUS_ACCESS`] is full.
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
}

/// One access surrogate a refresh re-minted, kept working until its own
/// expiry. See [`Surrogates::previous_access`].
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

/// Which surrogate a lookup is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SurrogateKind {
    Access,
    Refresh,
    ApiKey,
}

/// A decrypted grant.
#[derive(Clone)]
pub struct Grant {
    pub id: String,
    pub service: ServiceId,
    /// Incremented on every change of the secrets.
    pub version: i64,
    pub secrets: GrantSecrets,
}

/// Names the grant, never its secrets.
impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("id", &self.id)
            .field("service", &self.service)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// A new grant from an authorization-code exchange.
pub struct NewGrant {
    pub service: ServiceId,
    /// The provider's id of the account, for replacing an older grant of
    /// the same account and scopes. Stored only as an HMAC.
    pub account_id: String,
    /// Shown by `airlock show` (an email address).
    pub account_label: Option<String>,
    pub secrets: GrantSecrets,
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

/// One record of the `services.grants` database. The secrets are sealed; the rest
/// is plain.
#[derive(Serialize, Deserialize)]
struct GrantRecord {
    service: String,
    version: i64,
    /// HMAC of service, account id and scopes (hex): finds the grants a
    /// new sign-in replaces.
    identity: String,
    account: Option<String>,
    scopes: Vec<String>,
    /// ChaCha20-Poly1305 nonce and ciphertext of [`GrantSecrets`], base64.
    nonce: String,
    ciphertext: String,
    /// Unix ms of the API keys created in the last [`API_KEY_WINDOW_MS`].
    #[serde(default)]
    api_keys_created: Vec<i64>,
    created_at: i64,
    updated_at: i64,
}

/// One record of the `services.lookups` database, keyed by the HMAC of a
/// surrogate.
#[derive(Serialize, Deserialize)]
struct LookupRecord {
    grant: String,
    kind: SurrogateKind,
}

/// The store key from the vault, split into an encryption key and a
/// lookup key.
#[derive(Clone)]
struct Keys {
    encryption: [u8; 32],
    lookup: [u8; 32],
}

impl Keys {
    fn derive(master: &[u8; 32]) -> Self {
        Self {
            encryption: hmac_sha256(master, &[b"airlock service grants: encryption"]),
            lookup: hmac_sha256(master, &[b"airlock service grants: lookup"]),
        }
    }

    fn lookup(&self, service: ServiceId, surrogate: &str) -> [u8; 32] {
        hmac_sha256(
            &self.lookup,
            &[service.name().as_bytes(), b"\0", surrogate.as_bytes()],
        )
    }

    fn identity(&self, service: ServiceId, account_id: &str, scopes: &[String]) -> [u8; 32] {
        let mut scopes = scopes.to_vec();
        scopes.sort();
        hmac_sha256(
            &self.lookup,
            &[
                b"identity\0",
                service.name().as_bytes(),
                b"\0",
                account_id.as_bytes(),
                b"\0",
                scopes.join(" ").as_bytes(),
            ],
        )
    }

    /// The lookups of every surrogate of `secrets`.
    fn lookups_of(
        &self,
        service: ServiceId,
        secrets: &GrantSecrets,
    ) -> Vec<([u8; 32], SurrogateKind)> {
        let mut out = vec![(
            self.lookup(service, &secrets.surrogates.access),
            SurrogateKind::Access,
        )];
        for previous in &secrets.surrogates.previous_access {
            out.push((
                self.lookup(service, &previous.surrogate),
                SurrogateKind::Access,
            ));
        }
        if let Some(refresh) = &secrets.surrogates.refresh {
            out.push((self.lookup(service, refresh), SurrogateKind::Refresh));
        }
        for key in &secrets.api_keys {
            out.push((self.lookup(service, &key.surrogate), SurrogateKind::ApiKey));
        }
        out
    }

    fn seal(&self, id: &str, service: ServiceId, secrets: &GrantSecrets) -> anyhow::Result<Sealed> {
        let plain = serde_json::to_vec(secrets).context("serialize grant")?;
        let nonce = random_bytes::<12>()?;
        let ciphertext = ChaCha20Poly1305::new(<&Key>::from(&self.encryption))
            .encrypt(
                <&Nonce>::from(&nonce),
                Payload {
                    msg: &plain,
                    aad: &aad(id, service),
                },
            )
            .map_err(|_| anyhow!("encrypt grant"))?;
        Ok(Sealed {
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    fn open(
        &self,
        id: &str,
        service: ServiceId,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> anyhow::Result<GrantSecrets> {
        let nonce = <[u8; 12]>::try_from(nonce).map_err(|_| anyhow!("grant nonce length"))?;
        let plain = ChaCha20Poly1305::new(<&Key>::from(&self.encryption))
            .decrypt(
                <&Nonce>::from(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad(id, service),
                },
            )
            .map_err(|_| anyhow!("decrypt grant {id}: wrong key or corrupt data"))?;
        serde_json::from_slice(&plain).context("parse decrypted grant")
    }

    /// The secrets of `record` (grant `id`), decrypted.
    fn open_record(
        &self,
        id: &str,
        service: ServiceId,
        record: &GrantRecord,
    ) -> anyhow::Result<GrantSecrets> {
        let nonce = STANDARD_NO_PAD
            .decode(&record.nonce)
            .context("grant nonce")?;
        let ciphertext = STANDARD_NO_PAD
            .decode(&record.ciphertext)
            .context("grant ciphertext")?;
        self.open(id, service, &nonce, &ciphertext)
    }
}

struct Sealed {
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

fn aad(id: &str, service: ServiceId) -> Vec<u8> {
    format!("airlock-service-grant\0{id}\0{}", service.name()).into_bytes()
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

/// The databases of the store. Copied into each transaction.
#[derive(Clone, Copy)]
struct Tables {
    /// Grant id → [`GrantRecord`] (JSON).
    grants: Database<Str, Bytes>,
    /// HMAC of a surrogate → [`LookupRecord`] (JSON).
    lookups: Database<Bytes, Bytes>,
}

impl Tables {
    fn record(&self, txn: &RoTxn, id: &str) -> anyhow::Result<Option<GrantRecord>> {
        self.grants
            .get(txn, id)?
            .map(|bytes| serde_json::from_slice(bytes).context("parse a grant record"))
            .transpose()
    }

    fn put_record(&self, txn: &mut RwTxn, id: &str, record: &GrantRecord) -> anyhow::Result<()> {
        self.grants.put(txn, id, &serde_json::to_vec(record)?)?;
        Ok(())
    }

    /// The grant `id` of `service`, decrypted, with its record.
    fn grant(
        &self,
        txn: &RoTxn,
        keys: &Keys,
        id: &str,
        service: ServiceId,
    ) -> anyhow::Result<Option<(Grant, GrantRecord)>> {
        let Some(record) = self.record(txn, id)? else {
            return Ok(None);
        };
        if record.service != service.name() {
            return Ok(None);
        }
        let secrets = keys.open_record(id, service, &record)?;
        let grant = Grant {
            id: id.to_string(),
            service,
            version: record.version,
            secrets,
        };
        Ok(Some((grant, record)))
    }

    /// Seal the secrets and version of `grant` into `record`, and write
    /// it.
    fn put_grant(
        &self,
        txn: &mut RwTxn,
        keys: &Keys,
        grant: &Grant,
        mut record: GrantRecord,
    ) -> anyhow::Result<()> {
        let sealed = keys.seal(&grant.id, grant.service, &grant.secrets)?;
        record.version = grant.version;
        record.scopes.clone_from(&grant.secrets.scopes);
        record.nonce = STANDARD_NO_PAD.encode(sealed.nonce);
        record.ciphertext = STANDARD_NO_PAD.encode(sealed.ciphertext);
        record.updated_at = now_ms();
        self.put_record(txn, &grant.id, &record)
    }

    fn put_lookup(
        &self,
        txn: &mut RwTxn,
        lookup: &[u8; 32],
        grant: &str,
        kind: SurrogateKind,
    ) -> anyhow::Result<()> {
        let record = LookupRecord {
            grant: grant.to_string(),
            kind,
        };
        self.lookups
            .put(txn, lookup, &serde_json::to_vec(&record)?)?;
        Ok(())
    }

    /// Delete the grants in `ids` and every lookup that points to them.
    fn delete_grants(&self, txn: &mut RwTxn, ids: &[String]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut stale = Vec::new();
        for entry in self.lookups.iter(txn)? {
            let (lookup, value) = entry?;
            let record: LookupRecord =
                serde_json::from_slice(value).context("parse a lookup record")?;
            if ids.contains(&record.grant) {
                stale.push(lookup.to_vec());
            }
        }
        for lookup in &stale {
            self.lookups.delete(txn, lookup)?;
        }
        for id in ids {
            self.grants.delete(txn, id)?;
        }
        Ok(())
    }

    /// Replace the lookups of `grant` with the ones its secrets name now:
    /// used after a refresh re-minted some of them.
    fn rewrite_lookups(
        &self,
        txn: &mut RwTxn,
        keys: &Keys,
        before: &[([u8; 32], SurrogateKind)],
        grant: &Grant,
    ) -> anyhow::Result<()> {
        let after = keys.lookups_of(grant.service, &grant.secrets);
        for (lookup, _) in before {
            if !after.iter().any(|(l, _)| l == lookup) {
                self.lookups.delete(txn, lookup)?;
            }
        }
        for (lookup, kind) in &after {
            self.put_lookup(txn, lookup, &grant.id, *kind)?;
        }
        Ok(())
    }
}

/// The token store of one process (one per sandbox session) in the
/// airlock database. Creates its databases on first use.
pub struct TokenStore {
    db: Db,
    keys: Keys,
    tables: OnceCell<Tables>,
    /// Decrypted grants by id, and grant ids by lookup, of this process.
    cache: parking_lot::Mutex<Cache>,
    /// How long a cached grant is used ([`CACHE_TTL`]).
    cache_ttl: Duration,
}

#[derive(Default)]
struct Cache {
    /// Each grant with the time it was last read from the store.
    grants: HashMap<String, (Grant, Instant)>,
    lookups: HashMap<[u8; 32], (String, SurrogateKind)>,
}

impl Cache {
    fn evict(&mut self, id: &str) {
        self.grants.remove(id);
        self.lookups.retain(|_, (g, _)| g != id);
    }

    /// Cache `grant`, read from the store just now. An older version than
    /// the cached one only renews the cached one's time. Drops this
    /// grant's previously cached lookups first, so a lookup a refresh
    /// retired (not kept in [`Surrogates::previous_access`]) stops
    /// resolving from the cache too, not just from the store.
    fn insert(&mut self, keys: &Keys, grant: &Grant) {
        if let Some((cached, read)) = self.grants.get_mut(&grant.id)
            && cached.version > grant.version
        {
            *read = Instant::now();
            return;
        }
        self.lookups.retain(|_, (id, _)| id != &grant.id);
        for (lookup, kind) in keys.lookups_of(grant.service, &grant.secrets) {
            self.lookups.insert(lookup, (grant.id.clone(), kind));
        }
        self.grants
            .insert(grant.id.clone(), (grant.clone(), Instant::now()));
    }

    /// The cached grant of `lookup` (the hash of `surrogate`) and the
    /// kind of surrogate it is for, unless the grant was read `ttl` or
    /// longer ago, or its real access token has already expired (a
    /// refresh may already have replaced it elsewhere — this sends the
    /// lookup to the store instead of trusting the cached copy). For an
    /// access surrogate that is a kept [`Surrogates::previous_access`]
    /// one, also unless `surrogate` itself has already passed its own
    /// `exp`; the current access surrogate has no such deadline.
    fn find(
        &self,
        lookup: &[u8; 32],
        surrogate: &str,
        ttl: Duration,
    ) -> Option<(SurrogateKind, Grant)> {
        let (id, kind) = self.lookups.get(lookup)?;
        let (grant, read) = self.grants.get(id)?;
        let fresh = read.elapsed() < ttl && grant.secrets.access_expires_at > now_ms();
        let not_retired = *kind != SurrogateKind::Access
            || grant
                .secrets
                .previous_access_expires_at(surrogate)
                .is_none_or(|exp| exp > now_ms());
        (fresh && not_retired).then(|| (*kind, grant.clone()))
    }
}

impl TokenStore {
    /// A store in the database `db` with the vault's store key. Touches
    /// nothing yet.
    pub fn new(db: Db, master_key: &[u8; 32]) -> Self {
        Self {
            db,
            keys: Keys::derive(master_key),
            tables: OnceCell::new(),
            cache: parking_lot::Mutex::default(),
            cache_ttl: CACHE_TTL,
        }
    }

    /// The same store with another cache lifetime, for tests of a grant
    /// deleted by another process.
    #[cfg(test)]
    pub fn with_cache_ttl(mut self, ttl: Duration) -> Self {
        self.cache_ttl = ttl;
        self
    }

    /// The databases (created on first use) and the key, for a
    /// transaction.
    async fn tables(&self) -> anyhow::Result<(Tables, Keys)> {
        let tables = self
            .tables
            .get_or_try_init(|| async {
                anyhow::Ok(Tables {
                    grants: self.db.database(GRANTS).await?,
                    lookups: self.db.database(LOOKUPS).await?,
                })
            })
            .await?;
        Ok((*tables, self.keys.clone()))
    }

    /// Store a new grant and its surrogate lookups. Older grants of the
    /// same service, account and scopes are deleted in the same
    /// transaction: the new sign-in replaces them. Returns the new grant
    /// and the secrets of the replaced ones (to revoke upstream).
    pub async fn insert_grant(&self, new: NewGrant) -> anyhow::Result<(Grant, Vec<GrantSecrets>)> {
        let (tables, keys) = self.tables().await?;
        let id = hex::encode(random_bytes::<16>()?);
        let identity =
            hex::encode(keys.identity(new.service, &new.account_id, &new.secrets.scopes));
        let service = new.service.name();
        let grant = Grant {
            id,
            service: new.service,
            version: 1,
            secrets: new.secrets,
        };
        let now = now_ms();
        let record = GrantRecord {
            service: service.to_string(),
            version: 1,
            identity: identity.clone(),
            account: new.account_label,
            scopes: vec![],
            nonce: String::new(),
            ciphertext: String::new(),
            api_keys_created: vec![],
            created_at: now,
            updated_at: now,
        };
        let (grant, old, secrets) = self
            .db
            .write(move |txn| {
                let mut old = Vec::new();
                for entry in tables.grants.iter(txn)? {
                    let (old_id, bytes) = entry?;
                    let old_record: GrantRecord =
                        serde_json::from_slice(bytes).context("parse a grant record")?;
                    if old_record.service == service && old_record.identity == identity {
                        old.push(old_id.to_string());
                    }
                }
                let mut secrets = Vec::new();
                for id in &old {
                    // A record that does not decrypt has nothing to revoke.
                    if let Ok(Some((g, _))) = tables.grant(txn, &keys, id, grant.service) {
                        secrets.push(g.secrets);
                    }
                }
                tables.delete_grants(txn, &old)?;
                tables.put_grant(txn, &keys, &grant, record)?;
                for (lookup, kind) in keys.lookups_of(grant.service, &grant.secrets) {
                    tables.put_lookup(txn, &lookup, &grant.id, kind)?;
                }
                Ok((grant, old, secrets))
            })
            .await?;

        let mut cache = self.cache.lock();
        for id in &old {
            cache.evict(id);
        }
        cache.insert(&self.keys, &grant);
        Ok((grant, secrets))
    }

    /// The grant that issued `surrogate` as a `kind` surrogate, if any.
    /// Served from this process's cache when it read the grant less than
    /// [`CACHE_TTL`] ago and its real access token had not expired yet;
    /// [`Self::grant`] reads the record again regardless (a refresh
    /// relay must never act on a stale copy of the real refresh token).
    pub async fn find_by_surrogate(
        &self,
        service: ServiceId,
        kind: SurrogateKind,
        surrogate: &str,
    ) -> anyhow::Result<Option<Grant>> {
        let lookup = self.keys.lookup(service, surrogate);
        if let Some((cached, grant)) = self.cache.lock().find(&lookup, surrogate, self.cache_ttl) {
            return Ok((cached == kind).then_some(grant));
        }
        let (tables, keys) = self.tables().await?;
        // `gone`: the store has no grant for the lookup (any more).
        let (grant, gone) = self
            .db
            .read(move |txn| {
                let Some(bytes) = tables.lookups.get(txn, &lookup)? else {
                    return Ok((None, true));
                };
                let found: LookupRecord =
                    serde_json::from_slice(bytes).context("parse a lookup record")?;
                if found.kind != kind {
                    return Ok((None, false));
                }
                let grant = tables
                    .grant(txn, &keys, &found.grant, service)?
                    .map(|(grant, _)| grant);
                let gone = grant.is_none();
                Ok((grant, gone))
            })
            .await?;
        let mut cache = self.cache.lock();
        if let Some(grant) = &grant {
            cache.insert(&self.keys, grant);
        } else if gone && let Some((id, _)) = cache.lookups.get(&lookup).cloned() {
            // Signed out elsewhere: gone here too.
            cache.evict(&id);
        }
        // A previous access surrogate (see `Surrogates::previous_access`)
        // is kept only until its own `exp`; the store prunes it lazily,
        // on the next refresh, so a lookup must check that expiry itself
        // rather than trust the record's mere presence. The current
        // access surrogate has no such deadline: it still resolves after
        // the real access token has expired, since a 401 from the API is
        // the agent's own signal to refresh (see the module docs).
        Ok(grant.filter(|grant| {
            kind != SurrogateKind::Access
                || grant
                    .secrets
                    .previous_access_expires_at(surrogate)
                    .is_none_or(|exp| exp > now_ms())
        }))
    }

    /// The grant `id` of `service`, read directly from the store, never
    /// this process's cache: a refresh relay must act on the real
    /// refresh token as it stands now, never one another refresh (in
    /// this process or another) may already have rotated.
    pub async fn grant(&self, id: &str, service: ServiceId) -> anyhow::Result<Option<Grant>> {
        let (tables, keys) = self.tables().await?;
        let id = id.to_string();
        self.db
            .read(move |txn| Ok(tables.grant(txn, &keys, &id, service)?.map(|(g, _)| g)))
            .await
    }

    /// Record an API key created with the grant (and its surrogate). The
    /// oldest key goes when the grant has [`MAX_API_KEYS`].
    pub async fn add_api_key(
        &self,
        grant_id: &str,
        service: ServiceId,
        key: ApiKey,
    ) -> anyhow::Result<()> {
        let (tables, keys) = self.tables().await?;
        let grant_id = grant_id.to_string();
        let lookup = keys.lookup(service, &key.surrogate);
        let grant = self
            .db
            .write(move |txn| {
                let Some((mut grant, record)) = tables.grant(txn, &keys, &grant_id, service)?
                else {
                    bail!("the sign-in was removed");
                };
                grant.secrets.api_keys.push(key);
                let excess = grant.secrets.api_keys.len().saturating_sub(MAX_API_KEYS);
                for old in grant.secrets.api_keys.drain(..excess) {
                    tables
                        .lookups
                        .delete(txn, &keys.lookup(service, &old.surrogate))?;
                }
                grant.version += 1;
                tables.put_grant(txn, &keys, &grant, record)?;
                tables.put_lookup(txn, &lookup, &grant_id, SurrogateKind::ApiKey)?;
                Ok(grant)
            })
            .await?;
        // Rebuild the cached lookups: dropped keys must not stay findable.
        let mut cache = self.cache.lock();
        cache.evict(&grant.id);
        cache.insert(&self.keys, &grant);
        Ok(())
    }

    /// Count one API key creation with grant `grant_id`: `false` (and
    /// nothing counted) when the grant already created
    /// [`API_KEY_CREATIONS`] keys in the last [`API_KEY_WINDOW_MS`], or is
    /// gone.
    pub async fn take_api_key_slot(&self, grant_id: &str) -> anyhow::Result<bool> {
        let (tables, _) = self.tables().await?;
        let id = grant_id.to_string();
        self.db
            .write(move |txn| {
                let Some(mut record) = tables.record(txn, &id)? else {
                    return Ok(false);
                };
                let now = now_ms();
                record
                    .api_keys_created
                    .retain(|t| now - t < API_KEY_WINDOW_MS && *t <= now);
                if record.api_keys_created.len() >= API_KEY_CREATIONS {
                    return Ok(false);
                }
                record.api_keys_created.push(now);
                tables.put_record(txn, &id, &record)?;
                Ok(true)
            })
            .await
    }

    /// Delete the grant `grant_id` and its lookups (a sign-out). Deleting
    /// a grant that is gone is no error.
    pub async fn delete_grant(&self, grant_id: &str) -> anyhow::Result<()> {
        let (tables, _) = self.tables().await?;
        let ids = vec![grant_id.to_string()];
        self.db
            .write(move |txn| tables.delete_grants(txn, &ids))
            .await?;
        self.cache.lock().evict(grant_id);
        Ok(())
    }

    /// Apply a refresh relay's answer to the grant `grant_id`'s *current*
    /// record, in one write transaction: `mutate` (the provider's
    /// [`super::oauth::Provider::apply_refresh`]) gets the record as it stands now, not
    /// a value the caller read earlier (another refresh may have run
    /// meanwhile); the lookups of any surrogate it re-minted are rewritten
    /// in the same transaction, and the version is incremented.
    ///
    /// `Ok(None)`: the grant was deleted meanwhile (a sign-out during the
    /// upstream call); nothing is written, and the caller revokes the
    /// tokens it got instead of storing them.
    pub async fn replace_tokens<F>(
        &self,
        grant_id: &str,
        service: ServiceId,
        mutate: F,
    ) -> anyhow::Result<Option<Grant>>
    where
        F: FnOnce(&mut GrantSecrets) -> anyhow::Result<()> + Send + 'static,
    {
        let (tables, keys) = self.tables().await?;
        let id = grant_id.to_string();
        let grant = self
            .db
            .write(move |txn| {
                let Some((mut grant, record)) = tables.grant(txn, &keys, &id, service)? else {
                    return Ok(None);
                };
                let before = keys.lookups_of(grant.service, &grant.secrets);
                mutate(&mut grant.secrets)?;
                grant.version += 1;
                tables.put_grant(txn, &keys, &grant, record)?;
                tables.rewrite_lookups(txn, &keys, &before, &grant)?;
                Ok(Some(grant))
            })
            .await?;
        let mut cache = self.cache.lock();
        match &grant {
            Some(grant) => cache.insert(&self.keys, grant),
            None => cache.evict(grant_id),
        }
        Ok(grant)
    }
}

/// The grants stored in `db`, without the key: for `airlock show`.
pub async fn list_grants(db: &Db) -> anyhow::Result<Vec<GrantSummary>> {
    let grants = db.database::<Str, Bytes>(GRANTS).await?;
    db.read(move |txn| {
        let mut out = Vec::new();
        for entry in grants.iter(txn)? {
            let (_, bytes) = entry?;
            let record: GrantRecord =
                serde_json::from_slice(bytes).context("parse a grant record")?;
            out.push(GrantSummary {
                service: record.service,
                account: record.account,
                scopes: record.scopes,
                created_at: record.created_at,
            });
        }
        out.sort_by(|a, b| (&a.service, a.created_at).cmp(&(&b.service, b.created_at)));
        Ok(out)
    })
    .await
}

#[cfg(test)]
mod tests;
