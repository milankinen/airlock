//! Surrogate tokens: the shared engine that finds the real tokens in a
//! provider's answer and puts surrogates in their place.
//!
//! Each provider has a table of [`Format`]s ([`Formats`]): how to tell a
//! real token of the format, of which [`TokenKind`] it is, how to mint its
//! surrogate, and how to tell a surrogate of the format (its fixed
//! prefix). The engine knows no provider: an agent update that adds a
//! field to a token answer needs no change here, and a new token format
//! is one more row in the provider's table.
//!
//! On a token answer ([`collect`]):
//!
//! - every string a format recognizes, anywhere in the answer, is a real
//!   token: it is stored with the grant and the guest gets a surrogate
//!   ([`mint_all`] for a new grant, [`apply_refresh`] for a refresh;
//!   [`substitute`] puts the surrogates in). The standard token fields
//!   (`access_token`, `refresh_token`, `id_token`) are decided by their
//!   key, as the agent reads them: each provider table ends with a format
//!   per field that takes any value, after the formats of known shapes
//!   (whose minters keep claims or prefixes). A real refresh token of an
//!   unexpected format still gets a surrogate.
//! - fail closed: a string under another token-like key ([`is_token_key`],
//!   also deeper inside such a key) that no format recognizes, that is no
//!   surrogate of airlock, and that can be a credential (not an
//!   identifier key, not a UUID, number or boolean, at least
//!   [`MIN_CREDENTIAL_LEN`] characters) refuses the whole answer. The guest
//!   then gets a local `502` and nothing is stored: an unknown secret
//!   never reaches the sandbox.
//!
//! Surrogates have at least [`SURROGATE_BYTES`] random bytes from the
//! CSPRNG after their fixed prefix ([`surrogate`]); a fake JWT has them in
//! its nonce claim and again in its signature ([`fake_jwt`]).

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::auth_codes;
use super::store::{Grant, now_ms, random_bytes};

/// The random bytes of a surrogate after its prefix.
pub const SURROGATE_BYTES: usize = 48;

/// What every fake JWT starts with: its fixed header
/// `{"alg":"none","typ":"JWT"}` (base64url) and the dot. Real tokens never
/// use `alg` `none`.
pub const FAKE_JWT_PREFIX: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.";

/// How long a replaced access surrogate stays valid when its real token
/// had no known expiry.
const DEFAULT_PREVIOUS_LIFETIME_MS: i64 = 60 * 60 * 1000;

/// What a key that names a secret contains.
const SECRET_PARTS: &[&str] = &[
    "token",
    "secret",
    "key",
    "cred",
    "password",
    "passwd",
    "session",
    "jwt",
    "bearer",
    "cookie",
    "assertion",
];

/// Keys with "token" in their name whose values are no secret.
const METADATA_KEYS: &[&str] = &[
    "token_type",
    "token_type_hint",
    "issued_token_type",
    "requested_token_type",
    "subject_token_type",
];

/// What a token is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenKind {
    Access,
    Refresh,
    /// An OpenID Connect ID token: no credential, but it carries the
    /// account.
    Id,
    ApiKey,
}

impl TokenKind {
    /// The key of an OAuth token answer that holds the token of this
    /// kind.
    fn answer_key(self) -> Option<&'static str> {
        match self {
            TokenKind::Access => Some("access_token"),
            TokenKind::Refresh => Some("refresh_token"),
            TokenKind::Id => Some("id_token"),
            TokenKind::ApiKey => None,
        }
    }
}

/// One real token of a grant and its surrogate.
#[derive(Clone, Serialize, Deserialize)]
pub struct Token {
    pub kind: TokenKind,
    pub real: String,
    pub surrogate: String,
    /// When the real token expires (Unix ms), if known.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// One token format of a provider.
pub struct Format {
    pub kind: TokenKind,
    /// Whether `value`, found under the JSON key `key` (`""` when no key
    /// is known), is a real token of this format. Never called with a
    /// surrogate of the provider.
    pub recognize: fn(key: &str, value: &str) -> bool,
    /// Whether `value` is a surrogate of this format (its fixed prefix).
    pub is_surrogate: fn(value: &str) -> bool,
    /// A new surrogate for the real token `real`.
    pub mint: fn(real: &str) -> anyhow::Result<String>,
    /// What every real token of the format starts with, for the scan of
    /// API answers ([`super::scan`]); empty: the format has no fixed start
    /// (only the store's own copies of such tokens are found there).
    pub starts: &'static [&'static str],
    /// Whether a run of token characters that begins with one of
    /// [`Self::starts`] (at most [`super::scan::MAX_TOKEN_LEN`] bytes) is
    /// a real token of a realistic shape, for the scan of API answers. Not
    /// called with a surrogate. Stricter than [`Self::recognize`]: the
    /// scan sees text that is no token answer.
    pub shape: fn(run: &str) -> bool,
    /// The surrogate carries facts of its real token (a fake JWT carries
    /// its claims and `exp`): a new real token gets a new surrogate. Other
    /// surrogates stay for the life of the grant.
    pub carries_claims: bool,
}

/// The token formats of one provider.
pub struct Formats(pub &'static [Format]);

impl Formats {
    /// The format that recognizes `value` under `key`; `None` for
    /// surrogates and strings of no format.
    pub fn recognize(&self, key: &str, value: &str) -> Option<&'static Format> {
        if self.is_surrogate(value) {
            return None;
        }
        self.0.iter().find(|f| (f.recognize)(key, value))
    }

    /// Whether `value` is shaped as a surrogate of the provider (known to
    /// the store or not).
    pub fn is_surrogate(&self, value: &str) -> bool {
        self.0.iter().any(|f| (f.is_surrogate)(value))
    }

    /// Whether `value` is a real token of the provider wherever it is:
    /// what an API answer must not carry.
    pub fn is_real(&self, value: &str) -> bool {
        self.recognize("", value).is_some()
    }
}

/// Whether a JSON key names a secret (case-insensitive): it contains one
/// of [`SECRET_PARTS`], it is `code` or `authorization_code`, or it has
/// the word `auth` (`x_auth`, `authValue`; not `authorization_endpoint`).
/// Known metadata keys (`token_type`, …) and identifiers
/// ([`is_id_key`]) do not count. Broader than
/// `oauth::CODE_KEYS` and its token keys, which refuse any answer of an
/// allowed route that has such a field: here a recognized token under the
/// key is fine (it gets a surrogate), so the net can be wider.
pub fn is_token_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    if METADATA_KEYS.contains(&lower.as_str()) || is_id_key(key) {
        return false;
    }
    SECRET_PARTS.iter().any(|p| lower.contains(p))
        || lower == "code"
        || lower == "authorization_code"
        || words(key).any(|w| w == "auth")
}

/// Whether a key names an identifier, no secret: `id`, `uuid`, or a key
/// ending in `_id` or `_uuid` (`token_uuid`, `client_id`).
fn is_id_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "id" || key == "uuid" || key.ends_with("_id") || key.ends_with("_uuid")
}

/// The shortest string the fail-closed rule takes for a credential.
const MIN_CREDENTIAL_LEN: usize = 16;

/// Whether a string can be a credential: not shorter than
/// [`MIN_CREDENTIAL_LEN`], not a number, a boolean or a UUID.
fn can_be_credential(s: &str) -> bool {
    let uuid = s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    s.len() >= MIN_CREDENTIAL_LEN
        && !uuid
        && !s
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b'.' || c == b'-')
        && !matches!(s, "true" | "false")
}

/// The words of a key: split at non-alphanumeric characters and before an
/// uppercase letter that follows a lowercase one, lowercased.
fn words(key: &str) -> impl Iterator<Item = String> + '_ {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        let boundary = !c.is_ascii_alphanumeric() || (c.is_ascii_uppercase() && prev_lower);
        if boundary && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if c.is_ascii_alphanumeric() {
            word.push(c.to_ascii_lowercase());
        }
        prev_lower = c.is_ascii_lowercase();
    }
    if !word.is_empty() {
        words.push(word);
    }
    words.into_iter()
}

/// A real token found in an answer.
pub struct Found {
    pub kind: TokenKind,
    pub real: String,
    pub format: &'static Format,
    /// The token is the answer's own field of its kind (`access_token`,
    /// `refresh_token`, `id_token` at the top): the grant's main token of
    /// the kind.
    pub primary: bool,
    pub expires_at: Option<i64>,
}

/// Why an answer is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A string under this token-like key is in no known format.
    Unknown(String),
    /// A real token of a kind this answer must not carry.
    NotAllowed(TokenKind),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Unknown(key) => write!(f, "the value of {key:?} is in no known token format"),
            Refusal::NotAllowed(kind) => write!(f, "the answer carries a {kind:?} token"),
        }
    }
}

/// Find the real tokens of `answer` (see the module docs). `allowed`: the
/// kinds the answer may carry. `strict`: refuse an unknown string under a
/// token-like key (token answers); off for answers whose other fields are
/// not known (a created API key), where only known formats count.
pub fn collect(
    formats: &Formats,
    answer: &Map<String, Value>,
    allowed: &[TokenKind],
    strict: bool,
) -> Result<Vec<Found>, Refusal> {
    let expires_in = answer.get("expires_in").and_then(Value::as_i64);
    let mut found = Vec::new();
    for (key, value) in answer {
        let secret = strict && is_token_key(key);
        walk(formats, key, value, true, secret, strict, &mut found)?;
    }
    for f in &mut found {
        if !allowed.contains(&f.kind) {
            return Err(Refusal::NotAllowed(f.kind));
        }
        f.expires_at = match jwt_exp(&f.real) {
            Some(exp) => Some(exp * 1000),
            None if f.kind == TokenKind::Access => expires_in.map(|s| now_ms() + s * 1000),
            None => None,
        };
    }
    Ok(found)
}

/// One value of an answer under `key`; `top`: directly in the answer
/// object; `secret`: under a token-like key (here or above) of a
/// `strict` walk.
fn walk(
    formats: &Formats,
    key: &str,
    value: &Value,
    top: bool,
    secret: bool,
    strict: bool,
    found: &mut Vec<Found>,
) -> Result<(), Refusal> {
    match value {
        Value::String(s) => {
            if let Some(format) = formats.recognize(key, s) {
                found.push(Found {
                    kind: format.kind,
                    real: s.clone(),
                    format,
                    primary: top && format.kind.answer_key() == Some(key),
                    expires_at: None,
                });
            } else if secret
                && !is_id_key(key)
                && can_be_credential(s)
                && !formats.is_surrogate(s)
                && !s.starts_with(auth_codes::PREFIX)
            {
                return Err(Refusal::Unknown(key.to_string()));
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(formats, key, item, false, secret, strict, found)?;
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                let secret = secret || (strict && is_token_key(k));
                walk(formats, k, v, false, secret, strict, found)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The surrogates of the tokens of a new grant: one per distinct real
/// token, each kind's main token first.
pub fn mint_all(found: &[Found]) -> anyhow::Result<Vec<Token>> {
    let mut tokens: Vec<Token> = Vec::new();
    for f in ordered(found) {
        if tokens.iter().any(|t| t.real == f.real) {
            continue;
        }
        tokens.push(Token {
            kind: f.kind,
            real: f.real.clone(),
            surrogate: (f.format.mint)(&f.real)?,
            expires_at: f.expires_at,
        });
    }
    Ok(tokens)
}

/// `found` with each kind's main token first.
fn ordered(found: &[Found]) -> Vec<&Found> {
    let mut out: Vec<&Found> = found.iter().filter(|f| f.primary).collect();
    out.extend(found.iter().filter(|f| !f.primary));
    out
}

/// Apply the tokens of a refresh answer to `grant` in place. Each kind the
/// answer carries replaces the grant's tokens of that kind; kinds the
/// answer leaves out stay. The main token keeps its surrogate unless the
/// surrogate carries claims ([`Format::carries_claims`]); a replaced access
/// surrogate stays valid until the expiry of its own real token
/// ([`Grant::keep_previous_access`]). Returns real token → surrogate, for
/// [`substitute`].
pub fn apply_refresh(
    grant: &mut Grant,
    found: &[Found],
) -> anyhow::Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let kinds: Vec<TokenKind> = {
        let mut seen = HashSet::new();
        ordered(found)
            .iter()
            .map(|f| f.kind)
            .filter(|k| seen.insert(*k))
            .collect()
    };
    for kind in kinds {
        let old = grant.tokens.iter().find(|t| t.kind == kind).cloned();
        let mut new: Vec<Token> = Vec::new();
        for f in ordered(found).into_iter().filter(|f| f.kind == kind) {
            if new.iter().any(|t| t.real == f.real) {
                continue;
            }
            let keep = new.is_empty() && !f.format.carries_claims;
            let surrogate = match &old {
                Some(old) if keep && (f.format.is_surrogate)(&old.surrogate) => {
                    old.surrogate.clone()
                }
                _ => (f.format.mint)(&f.real)?,
            };
            new.push(Token {
                kind,
                real: f.real.clone(),
                surrogate,
                expires_at: f.expires_at,
            });
        }
        if kind == TokenKind::Access
            && let Some(old) = &old
            && new.first().is_some_and(|n| n.surrogate != old.surrogate)
        {
            let expires_at = old
                .expires_at
                .unwrap_or_else(|| now_ms() + DEFAULT_PREVIOUS_LIFETIME_MS);
            grant.keep_previous_access(old.surrogate.clone(), expires_at);
        }
        for t in &new {
            map.insert(t.real.clone(), t.surrogate.clone());
        }
        grant.tokens.retain(|t| t.kind != kind);
        grant.tokens.extend(new);
    }
    Ok(map)
}

/// Replace every string of `value` that is a key of `map` by its value.
pub fn substitute(value: &mut Value, map: &HashMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(surrogate) = map.get(s.as_str()) {
                s.clone_from(surrogate);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| substitute(v, map)),
        Value::Object(obj) => obj.values_mut().for_each(|v| substitute(v, map)),
        _ => {}
    }
}

/// A random surrogate: `prefix` and [`SURROGATE_BYTES`] bytes from the
/// CSPRNG, base64url.
pub fn surrogate(prefix: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(random_bytes::<SURROGATE_BYTES>()?)
    ))
}

/// A fake unpadded JWT with the claims of the JWT `real` (its own `exp`
/// included), a random nonce claim and a random signature. `None` when
/// `real` is not a JWT with a claims object.
pub fn fake_jwt(real: &str) -> anyhow::Result<Option<String>> {
    let Some(Value::Object(mut claims)) = jwt_claims(real) else {
        return Ok(None);
    };
    claims.insert(
        "airlock_nonce".into(),
        URL_SAFE_NO_PAD.encode(random_bytes::<32>()?).into(),
    );
    let payload = URL_SAFE_NO_PAD.encode(Value::Object(claims).to_string());
    let signature = URL_SAFE_NO_PAD.encode(random_bytes::<32>()?);
    Ok(Some(format!("{FAKE_JWT_PREFIX}{payload}.{signature}")))
}

/// The claims of a JWT (unverified): the decoded middle part.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The JWT at the start of a run of token characters: its first three
/// dot-separated parts. `None` with fewer parts.
pub fn jwt_at_start(run: &str) -> Option<&str> {
    match run.match_indices('.').nth(2) {
        Some((end, _)) => Some(&run[..end]),
        None => (run.matches('.').count() == 2).then_some(run),
    }
}

/// The `exp` claim of a JWT (Unix seconds).
fn jwt_exp(token: &str) -> Option<i64> {
    jwt_claims(token)?.get("exp")?.as_i64()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn is_real(_key: &str, v: &str) -> bool {
        v.starts_with("real-")
    }
    fn is_ours(v: &str) -> bool {
        v.starts_with("ours-")
    }
    fn mint(_real: &str) -> anyhow::Result<String> {
        surrogate("ours-")
    }
    fn is_claimed(key: &str, v: &str) -> bool {
        key == "id_token" && v.starts_with("jwt-")
    }

    static FORMATS: Formats = Formats(&[
        Format {
            kind: TokenKind::Access,
            recognize: |k, v| k != "refresh_token" && is_real(k, v),
            is_surrogate: is_ours,
            mint,
            starts: &[],
            shape: |_| false,
            carries_claims: false,
        },
        Format {
            kind: TokenKind::Refresh,
            recognize: |k, v| k == "refresh_token" && is_real(k, v),
            is_surrogate: is_ours,
            mint,
            starts: &[],
            shape: |_| false,
            carries_claims: false,
        },
        Format {
            kind: TokenKind::Id,
            recognize: is_claimed,
            is_surrogate: is_ours,
            mint,
            starts: &[],
            shape: |_| false,
            carries_claims: true,
        },
    ]);

    const ALL: &[TokenKind] = &[TokenKind::Access, TokenKind::Refresh, TokenKind::Id];

    fn object(v: &Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn credential_like_key_is_token_key_and_metadata_key_is_not() {
        for key in [
            "access_token",
            "Refresh_Token",
            "client_secret",
            "raw_key",
            "code",
            "authorization_code",
            "tokens",
            "credentials",
            "Password",
            "id_jwt",
            "bearer",
            "set_cookie",
            "client_assertion",
            "x_auth",
            "authValue",
            "auth",
        ] {
            assert!(is_token_key(key), "{key}");
        }
        for key in [
            "token_type",
            "scope",
            "expires_in",
            "account",
            "codes",
            "authorization_endpoint",
            "author",
            "token_uuid",
            "session_id",
            "id",
        ] {
            assert!(!is_token_key(key), "{key}");
        }
    }

    #[test]
    fn collect_finds_tokens_anywhere_and_refuses_unknown_credentials() {
        let answer = object(&json!({
            "access_token": "real-a",
            "refresh_token": "real-r",
            "token_type": "Bearer",
            "expires_in": 60,
            "extra": { "nested": ["real-b"] },
            "name": "plain",
            "code": "airlock-code-x",
            "session_token": "ours-x",
        }));
        let found = collect(&FORMATS, &answer, ALL, true).unwrap();
        let mut got: Vec<(TokenKind, &str, bool)> = found
            .iter()
            .map(|f| (f.kind, f.real.as_str(), f.primary))
            .collect();
        got.sort_by_key(|g| g.1);
        assert_eq!(
            got,
            [
                (TokenKind::Access, "real-a", true),
                (TokenKind::Access, "real-b", false),
                (TokenKind::Refresh, "real-r", true),
            ]
        );
        let a = found.iter().find(|f| f.real == "real-a").unwrap();
        assert!(a.expires_at.unwrap() > now_ms());

        for bad in [
            json!({ "access_token": "opaque-secret-value-0123456789" }),
            json!({ "tokens": { "a": "opaque-secret-value-0123456789" } }),
            json!({ "data": [{ "client_secret": "opaque-secret-value-0123456789" }] }),
            json!({ "code": "opaque-secret-value-0123456789" }),
        ] {
            let got = collect(&FORMATS, &object(&bad), ALL, true);
            assert!(matches!(got, Err(Refusal::Unknown(_))), "{bad}");
            assert!(
                collect(&FORMATS, &object(&bad), ALL, false).is_ok(),
                "{bad}"
            );
        }
        for ok in [
            json!({ "token_uuid": "opaque-secret-value-0123456789" }),
            json!({ "session": { "id": "opaque-secret-value-0123456789" } }),
            json!({ "access_token": "short" }),
            json!({ "token_uuid": "c6c1b3c4-6f0e-4f43-9b1d-2a5e8f0d9a11" }),
            json!({ "secret": "12345678901234567890" }),
            json!({ "secret_flag": "true" }),
        ] {
            assert!(collect(&FORMATS, &object(&ok), ALL, true).is_ok(), "{ok}");
        }
        let got = collect(
            &FORMATS,
            &object(&json!({ "id_token": "jwt-1" })),
            &[TokenKind::Access],
            true,
        );
        assert_eq!(got.err(), Some(Refusal::NotAllowed(TokenKind::Id)));
    }

    #[test]
    fn refresh_keeps_opaque_surrogates_and_remints_ones_with_claims() {
        let found = collect(
            &FORMATS,
            &object(
                &json!({ "access_token": "real-a", "refresh_token": "real-r", "id_token": "jwt-1" }),
            ),
            ALL,
            true,
        )
        .unwrap();
        let mut grant = Grant::for_tests(mint_all(&found).unwrap());
        let before: Vec<String> = grant.tokens.iter().map(|t| t.surrogate.clone()).collect();
        assert_eq!(before.len(), 3);
        assert!(before.iter().all(|s| s.len() >= "ours-".len() + 64));

        let found = collect(
            &FORMATS,
            &object(&json!({ "access_token": "real-a2", "id_token": "jwt-2" })),
            ALL,
            true,
        )
        .unwrap();
        let map = apply_refresh(&mut grant, &found).unwrap();
        let token = |kind| {
            grant
                .tokens
                .iter()
                .find(|t| t.kind == kind)
                .unwrap()
                .clone()
        };
        assert_eq!(token(TokenKind::Access).real, "real-a2");
        assert_eq!(token(TokenKind::Access).surrogate, before[0], "stable");
        assert_eq!(token(TokenKind::Refresh).real, "real-r", "left out: kept");
        assert_ne!(token(TokenKind::Id).surrogate, before[2], "re-minted");
        assert!(grant.previous_access.is_empty());
        let mut answer = json!({ "access_token": "real-a2", "id_token": "jwt-2", "x": 1 });
        substitute(&mut answer, &map);
        assert_eq!(answer["access_token"], before[0]);
        assert_eq!(answer["id_token"], token(TokenKind::Id).surrogate);
    }

    #[test]
    fn fake_jwt_copies_real_claims_and_is_unique() {
        let real = format!(
            "eyJhbGciOiJSUzI1NiJ9.{}.sig",
            URL_SAFE_NO_PAD.encode(json!({ "exp": 7, "sub": "u" }).to_string())
        );
        let a = fake_jwt(&real).unwrap().unwrap();
        let b = fake_jwt(&real).unwrap().unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(FAKE_JWT_PREFIX));
        let claims = jwt_claims(&a).unwrap();
        assert_eq!(claims["exp"], 7);
        assert_eq!(claims["sub"], "u");
        assert!(fake_jwt("opaque").unwrap().is_none());
    }
}
