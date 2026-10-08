//! Surrogate tokens.
//!
//! Finds the real tokens in a token answer of a provider and puts
//! surrogates in their place, for new sign-ins and for refreshes. The
//! engine refuses an answer that can contain a secret in an unknown
//! format.
//!
//! Each provider gives a table of its token formats. The engine itself
//! knows no provider. A new field in a token answer needs no change, and a
//! new token format is one more row in the table of the provider.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::auth_codes;
use super::store::{Grant, now_ms, random_bytes};

/// Number of random bytes from the CSPRNG in a surrogate, after its fixed
/// prefix.
pub const SURROGATE_BYTES: usize = 48;

/// Start of every fake JWT: its fixed header `{"alg":"none","typ":"JWT"}`
/// (base64url) and the dot. Real tokens never use `alg` `none`.
pub const FAKE_JWT_PREFIX: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.";

/// How long a replaced access surrogate stays valid if the expiry of its
/// real token is not known.
const DEFAULT_PREVIOUS_LIFETIME_MS: i64 = 60 * 60 * 1000;

/// Parts of a key name that make the key name a secret.
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

/// Keys with "token" in their name whose values are not secret.
const METADATA_KEYS: &[&str] = &[
    "token_type",
    "token_type_hint",
    "issued_token_type",
    "requested_token_type",
    "subject_token_type",
];

/// The purpose of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenKind {
    /// An OAuth access token.
    Access,
    /// An OAuth refresh token.
    Refresh,
    /// An OpenID Connect ID token. It is not a credential, but it contains
    /// the account.
    Id,
    /// An API key that a grant created.
    ApiKey,
}

impl TokenKind {
    /// Key of an OAuth token answer that holds the token of this kind.
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
    /// The token kind.
    pub kind: TokenKind,
    /// The real token from the provider.
    pub real: String,
    /// The surrogate that the sandbox gets.
    pub surrogate: String,
    /// When the real token expires (Unix ms), if known.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// One token format of a provider.
///
/// A provider table lists the formats of known shapes first. Their
/// minters keep claims or prefixes. The table ends with one format per
/// standard token field (`access_token`, `refresh_token`, `id_token`)
/// that accepts any value: the key decides, as the agent reads it. Thus a
/// real refresh token of an unexpected format still gets a surrogate.
pub struct Format {
    /// The kind of the tokens of this format.
    pub kind: TokenKind,
    /// Whether `value`, found under the JSON key `key` (`""` if the key is
    /// not known), is a real token of this format. Never called with a
    /// surrogate of the provider.
    pub recognize: fn(key: &str, value: &str) -> bool,
    /// Whether `value` is a surrogate of this format (by its fixed
    /// prefix).
    pub is_surrogate: fn(value: &str) -> bool,
    /// Make a new surrogate for the real token `real`.
    pub mint: fn(real: &str) -> anyhow::Result<String>,
    /// Start of every real token of the format, for the scan of API
    /// answers ([`super::scan`]). Empty if the format has no fixed start.
    /// Then the scan finds only the tokens that the store holds.
    pub starts: &'static [&'static str],
    /// Whether a run of token characters that starts with one of
    /// [`Self::starts`] (at most [`super::scan::MAX_TOKEN_LEN`] bytes) is
    /// a real token of a realistic shape, for the scan of API answers. Not
    /// called with a surrogate. Stricter than [`Self::recognize`], because
    /// the scan sees text that is not a token answer.
    pub shape: fn(run: &str) -> bool,
    /// Whether the surrogate contains facts of its real token (a fake JWT
    /// contains its claims and `exp`). If true, a new real token gets a new
    /// surrogate. Other surrogates stay for the life of the grant.
    pub carries_claims: bool,
}

/// The token formats of one provider, in match order.
pub struct Formats(pub &'static [Format]);

impl Formats {
    /// Find the format that recognizes `value` under `key`. Returns `None`
    /// for surrogates and for strings of no format.
    pub fn recognize(&self, key: &str, value: &str) -> Option<&'static Format> {
        if self.is_surrogate(value) {
            return None;
        }
        self.0.iter().find(|f| (f.recognize)(key, value))
    }

    /// Whether `value` has the shape of a surrogate of the provider (known
    /// to the store or not).
    pub fn is_surrogate(&self, value: &str) -> bool {
        self.0.iter().any(|f| (f.is_surrogate)(value))
    }

    /// Whether `value` is a real token of the provider, without regard to
    /// its key. An API answer must not contain such a value.
    pub fn is_real(&self, value: &str) -> bool {
        self.recognize("", value).is_some()
    }
}

/// Whether a JSON key names a secret (case-insensitive).
///
/// A key names a secret if it contains one of [`SECRET_PARTS`], if it is
/// `code` or `authorization_code`, or if it has the word `auth`
/// (`x_auth`, `authValue`, but not `authorization_endpoint`). Known
/// metadata keys (`token_type`, …) and identifiers ([`is_id_key`]) do not
/// count.
///
/// This set is broader than the token and code keys of
/// [`super::oauth::backstop`], which refuses any answer with such a field.
/// Here a recognized token under the key is accepted (it gets a
/// surrogate), so the set can be wider.
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

/// Whether a key names an identifier, not a secret: `id`, `uuid`, or a key
/// that ends in `_id` or `_uuid` (`token_uuid`, `client_id`).
fn is_id_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "id" || key == "uuid" || key.ends_with("_id") || key.ends_with("_uuid")
}

/// Minimum length of a string that the fail-closed rule of [`collect`]
/// treats as a possible credential.
const MIN_CREDENTIAL_LEN: usize = 16;

/// Whether a string can be a credential: at least [`MIN_CREDENTIAL_LEN`]
/// long, and not a number, a boolean or a UUID.
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

/// Split a key into lowercase words. A word ends at a non-alphanumeric
/// character and before an uppercase letter that follows a lowercase
/// letter.
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
    /// The token kind.
    pub kind: TokenKind,
    /// The real token.
    pub real: String,
    /// The format that recognized the token.
    pub format: &'static Format,
    /// True if the token is the answer's own field of its kind
    /// (`access_token`, `refresh_token` or `id_token` at the top level).
    /// Then it is the grant's main token of that kind.
    pub primary: bool,
    /// When the real token expires (Unix ms), if known.
    pub expires_at: Option<i64>,
}

/// The reason why an answer is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A string under this token-like key is in no known format.
    Unknown(String),
    /// A real token of a kind that this answer must not contain.
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

/// Find the real tokens of a provider answer.
///
/// Every string that a format recognizes, anywhere in the answer, is a
/// real token. The caller stores it with the grant and gives the guest a
/// surrogate.
///
/// In strict mode, the answer fails closed: a string that can be a
/// credential under a token-like key ([`is_token_key`], also deeper below
/// such a key) refuses the whole answer, if no format recognizes it and
/// it is not a surrogate of airlock. An identifier key, a UUID, a number,
/// a boolean or a string shorter than [`MIN_CREDENTIAL_LEN`] is not a
/// credential. Thus an unknown secret never gets to the sandbox.
/// Args:
///  - `formats`: Token formats of the provider
///  - `answer`: The JSON answer
///  - `allowed`: Token kinds that the answer can contain
///  - `strict`: Refuse an unknown string under a token-like key. Use it
///    for token answers. Do not use it for answers whose other fields are
///    not known (a created API key), where only known formats count.
///
/// Returns:
///   The real tokens found, or the reason to refuse the answer.
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

/// Walk one value of an answer under `key` for [`collect`].
///
/// `top` is true for a value directly in the answer object. `secret` is
/// true below a token-like key (here or above) in a `strict` walk.
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

/// Make the surrogates of the tokens of a new grant: one per distinct real
/// token, with the main token of each kind first.
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

/// Sort `found` so that the main token of each kind comes first.
fn ordered(found: &[Found]) -> Vec<&Found> {
    let mut out: Vec<&Found> = found.iter().filter(|f| f.primary).collect();
    out.extend(found.iter().filter(|f| !f.primary));
    out
}

/// Apply the tokens of a refresh answer to `grant` in place.
///
/// Each kind in the answer replaces the grant's tokens of that kind. Kinds
/// that the answer does not contain stay. The main token keeps its
/// surrogate, unless the surrogate contains claims
/// ([`Format::carries_claims`]). A replaced access surrogate stays valid
/// until the expiry of its own real token
/// ([`Grant::keep_previous_access`]).
/// Returns:
///   A map from real token to surrogate, for [`substitute`].
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

/// Replace every string of `value` that is a key of `map` with the map
/// value.
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

/// Make a random surrogate: `prefix` and [`SURROGATE_BYTES`] bytes from
/// the CSPRNG, base64url.
pub fn surrogate(prefix: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(random_bytes::<SURROGATE_BYTES>()?)
    ))
}

/// Make a fake unpadded JWT from the JWT `real`.
///
/// The fake JWT has the claims of `real` (also its `exp`), a random nonce
/// claim and a random signature.
/// Returns:
///   The fake JWT, or `None` if `real` is not a JWT with a claims object.
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

/// Get the claims of a JWT (not verified): the decoded middle part.
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

/// Get the JWT at the start of a run of token characters: its first three
/// dot-separated parts. Returns `None` if there are fewer parts.
pub fn jwt_at_start(run: &str) -> Option<&str> {
    match run.match_indices('.').nth(2) {
        Some((end, _)) => Some(&run[..end]),
        None => (run.matches('.').count() == 2).then_some(run),
    }
}

/// Get the `exp` claim of a JWT (Unix seconds).
fn jwt_exp(token: &str) -> Option<i64> {
    jwt_claims(token)?.get("exp")?.as_i64()
}

#[cfg(test)]
mod tests {
    //! Tokens in provider answers: token keys, the collection of tokens,
    //! refresh of surrogates and fake JWTs.

    use serde_json::json;

    use super::*;

    // A test provider: real tokens start with `real-`, surrogates with
    // `ours-`, and ID tokens with `jwt-` carry claims.

    /// True for a real token of the test provider.
    fn is_real(_key: &str, v: &str) -> bool {
        v.starts_with("real-")
    }
    /// True for a surrogate of the test provider.
    fn is_ours(v: &str) -> bool {
        v.starts_with("ours-")
    }
    /// Mint a surrogate of the test provider.
    fn mint(_real: &str) -> anyhow::Result<String> {
        surrogate("ours-")
    }
    /// True for an ID token of the test provider.
    fn is_claimed(key: &str, v: &str) -> bool {
        key == "id_token" && v.starts_with("jwt-")
    }

    /// Token formats of the test provider.
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

    /// All token kinds.
    const ALL: &[TokenKind] = &[TokenKind::Access, TokenKind::Refresh, TokenKind::Id];

    /// The JSON object of `v`.
    fn object(v: &Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// Test that a key that names a credential is a token key and that a
    /// metadata key is not.
    ///   1. Check that credential keys in different cases and styles are token
    ///      keys
    ///   2. Check that metadata and identifier keys are not token keys
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

    /// Test that the collection finds real tokens at all depths and refuses an
    /// unknown credential in strict mode. An unknown secret must never get to
    /// the sandbox.
    ///   1. Collect from an answer with top-level and nested tokens, a
    ///      surrogate code and a surrogate
    ///   2. Check the found tokens, which ones are main tokens, and the expiry
    ///   3. Check that unknown values under token keys are refused only in
    ///      strict mode
    ///   4. Check that identifiers, short values, numbers and flags pass
    ///   5. Check that a token kind that is not allowed is refused
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

    /// Test that a refresh keeps the surrogates of opaque tokens and mints new
    /// surrogates for tokens with claims. A surrogate with claims must show
    /// the new claims.
    ///   1. Make a grant from an answer with access, refresh and ID tokens
    ///   2. Apply a refresh answer with a new access token and a new ID token
    ///   3. Check that the access surrogate stays, the refresh token stays and
    ///      the ID surrogate changes
    ///   4. Replace the real tokens in the answer and check the surrogates
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

    /// Test that a fake JWT copies the claims of the real JWT and is different
    /// each time.
    ///   1. Make two fake JWTs of one real JWT
    ///   2. Check that they differ, have the prefix and contain the claims
    ///   3. Check that an opaque token gives no fake JWT
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
