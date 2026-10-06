//! The `openai` service: Codex's ChatGPT sign-in (`codex login`) and its
//! use of chatgpt.com.
//!
//! On the auth host (`auth.openai.com`), matched on the normalized path
//! (see [`oauth::normalize_path`]):
//!
//! - `POST /oauth/token` (parsed strictly, see [`oauth`]):
//!   - form `authorization_code` (browser and device-code sign-in): the
//!     `code` must be a surrogate code ([`super::auth_codes`]) issued to
//!     this service through the channel of the `redirect_uri` (the
//!     loopback callback port, or the device flow's
//!     `https://auth.openai.com/deviceauth/callback`); it is swapped for
//!     the real code and the exchange forwarded. The answer's
//!     real `id_token`, `access_token` and `refresh_token` are stored as a
//!     new grant. The guest gets fake unpadded JWTs with the real tokens'
//!     own claims (`exp` included, a random nonce, a random signature) and
//!     an `airlock-rt-…` refresh surrogate. Codex checks no JWT signature.
//!   - form token exchange (an API key for the ID token): refused locally
//!     with `unsupported_grant_type`; Codex treats that as non-fatal.
//!   - JSON `refresh_token`: Codex's own refresh, relayed
//!     ([`oauth::Grants::relay_refresh`]); an unknown refresh surrogate
//!     gets `401 refresh_token_invalidated`. The access and ID-token fake
//!     JWTs are re-minted with the real tokens' new `exp`; the access
//!     surrogate they replace keeps working until its own `exp` (see
//!     [`super::store::Surrogates::previous_access`]).
//!   - other grant types: refused locally (`unsupported_grant_type`).
//! - `POST /api/accounts/deviceauth/token` (device-code sign-in): the
//!   `authorization_code` of the answer becomes a surrogate code.
//! - `POST /oauth/revoke`: a refresh or access surrogate deletes its grant
//!   and revokes both real tokens upstream (refresh, then access); the
//!   guest gets `200 {}`. `codex login` revokes before every sign-in, so
//!   a new sign-in signs the earlier grant out (as it does on a host).
//! - Everything else passes [`oauth::backstop`], without a token swap.
//!
//! On chatgpt.com, paths under `/backend-api/` (HTTP and the WebSocket
//! upgrade) whose raw path is already canonical (`normalize_path(p) ==
//! p`; the upstream may read another spelling differently), with strict
//! credentials (see [`super`]): `Authorization: Bearer <surrogate>` gets
//! the real token, an injected masked secret passes, anything else gets
//! a local `401`; a 401 from upstream passes through unchanged (Codex
//! refreshes itself). Answers pass [`oauth::api_backstop`]. Other paths,
//! and any host the service does not know, pass [`oauth::backstop`]
//! without a swap.
//!
//! Protocol facts: Codex 0.158.0.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::LocalBoxFuture;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};

use super::ServiceId;
use super::auth_codes::{Channel, PendingCodes};
use super::oauth::{self, Grants, Provider, TokenRequest, Upstream};
use super::sign_in::SignInPage;
use super::store::{Grant, GrantSecrets, NewGrant, Surrogates, TokenStore, now_ms};
use crate::network::http::ResponseBody;
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::{Endpoint, InjectedSecret, NetworkTarget};

/// Codex's OAuth client id.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

const TOKEN_PATH: &str = "/oauth/token";
const REVOKE_PATH: &str = "/oauth/revoke";
const DEVICE_TOKEN_PATH: &str = "/api/accounts/deviceauth/token";
/// The `redirect_uri` of the device-code sign-in's exchange.
const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";
/// The API paths on chatgpt.com that get the real token.
const API_PATH_PREFIX: &str = "/backend-api/";

const REFRESH_PREFIX: &str = "airlock-rt-";
const ACCESS_PREFIX: &str = "airlock-at-";

/// The scopes `codex login` asks for.
const SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "api.connectors.read",
    "api.connectors.invoke",
];

/// The pages a sign-in callback may send the browser to: the authorize
/// host and ChatGPT. Codex's own success page is on the loopback origin.
const PAGES: &[&str] = &["auth.openai.com", "chatgpt.com"];

/// Expiry of a real access token that is no JWT (or has no `exp`).
const DEFAULT_ACCESS_LIFETIME_MS: i64 = 60 * 60 * 1000;

/// Where the service's hosts are.
pub struct Endpoints {
    /// Sign-in, token endpoint and revoke (`auth.openai.com`).
    pub auth: Endpoint,
    /// The ChatGPT backend Codex talks to (`chatgpt.com`).
    pub chatgpt: Endpoint,
}

impl Endpoints {
    pub fn production() -> Self {
        Self {
            auth: Endpoint::new("auth.openai.com", 443),
            chatgpt: Endpoint::new("chatgpt.com", 443),
        }
    }

    pub fn targets(&self) -> Vec<NetworkTarget> {
        crate::network::target::targets_of(&[&self.auth, &self.chatgpt])
    }
}

/// The sign-in page. Codex listens on 127.0.0.1:1455, or 1457 when that
/// port is taken.
pub fn sign_in_pages() -> Vec<SignInPage> {
    vec![SignInPage {
        host: "auth.openai.com",
        path: "/oauth/authorize",
        client_id: CLIENT_ID,
        scopes: SCOPES,
        callback_ports: &[1455..=1455, 1457..=1457],
        callback_path: "/auth/callback",
        pages: PAGES,
    }]
}

fn is_access_surrogate(s: &str) -> bool {
    s.starts_with(oauth::FAKE_JWT_PREFIX) || s.starts_with(ACCESS_PREFIX)
}

fn is_refresh_surrogate(s: &str) -> bool {
    s.starts_with(REFRESH_PREFIX)
}

fn is_token_surrogate(s: &str) -> bool {
    is_access_surrogate(s) || is_refresh_surrogate(s)
}

/// Refresh and revoke at OpenAI.
struct OpenaiOauth;

static PROVIDER: OpenaiOauth = OpenaiOauth;

impl Provider for OpenaiOauth {
    fn id(&self) -> ServiceId {
        ServiceId::Openai
    }

    fn token_path(&self) -> &'static str {
        TOKEN_PATH
    }

    fn revoke_path(&self) -> &'static str {
        REVOKE_PATH
    }

    fn revoke_body(&self, token: &str, hint: &str) -> Value {
        json!({ "token": token, "token_type_hint": hint, "client_id": CLIENT_ID })
    }

    /// Codex falls back to revoking the access token: the endpoint takes
    /// both.
    fn revokes_access_tokens(&self) -> bool {
        true
    }

    /// Every field of the answer is optional; the refresh surrogate
    /// always stays. A new access token gets a re-minted fake JWT (its
    /// own `exp`), or an opaque surrogate when it is not a JWT with an
    /// `exp` ([`access_surrogate`], as for a code exchange); the
    /// surrogate it replaces keeps working until its own `exp`
    /// ([`Surrogates::keep_previous_access`]). A new ID token gets a
    /// re-minted fake JWT the same way, with no previous-surrogate
    /// bookkeeping (it is not an authorization credential).
    fn apply_refresh(
        &self,
        secrets: &mut GrantSecrets,
        answer: &Map<String, Value>,
    ) -> anyhow::Result<()> {
        let token = |name: &str| answer.get(name).and_then(Value::as_str).map(String::from);
        if let Some(access) = token("access_token") {
            let new_surrogate = access_surrogate(&access)?;
            let (old_surrogate, old_exp) =
                (secrets.surrogates.access.clone(), secrets.access_expires_at);
            if new_surrogate != old_surrogate {
                secrets
                    .surrogates
                    .keep_previous_access(old_surrogate, old_exp);
            }
            secrets.surrogates.access = new_surrogate;
            secrets.access_expires_at = access_expiry(&access);
            secrets.access_token = access;
        }
        if let Some(refresh) = token("refresh_token") {
            secrets.refresh_token = Some(refresh);
        }
        if let Some(id_token) = token("id_token") {
            if let Some(fake) = remint(&id_token)? {
                secrets.surrogates.id_token = Some(fake);
            }
            secrets.id_token = Some(id_token);
        }
        Ok(())
    }

    fn surrogate_answer(&self, grant: &Grant, answer: Map<String, Value>) -> Map<String, Value> {
        insert_surrogates(answer, grant)
    }
}

pub struct Openai {
    endpoints: Endpoints,
    grants: Grants,
    codes: PendingCodes,
    targets: Vec<NetworkTarget>,
}

impl Openai {
    pub fn new(
        endpoints: Endpoints,
        store: Arc<TokenStore>,
        tls: Arc<rustls::ClientConfig>,
        codes: PendingCodes,
    ) -> Self {
        Self {
            grants: Grants::new(&PROVIDER, store, Upstream::new(tls, endpoints.auth.clone())),
            codes,
            targets: endpoints.targets(),
            endpoints,
        }
    }

    async fn handle(
        &self,
        to: &Endpoint,
        mut req: Request<ResponseBody>,
        injected: &[InjectedSecret],
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        oauth::pin_authority(&mut req, to)?;
        let raw_path = req.uri().path();
        let path = oauth::normalize_path(raw_path);
        // The swap needs the path as the upstream reads it: only a
        // canonical one.
        let api =
            *to == self.endpoints.chatgpt && path == raw_path && path.starts_with(API_PATH_PREFIX);
        let post = req.method() == Method::POST;
        if *to == self.endpoints.auth {
            match path.as_str() {
                TOKEN_PATH if post => return self.token(req, next).await,
                REVOKE_PATH if post => {
                    return match TokenRequest::read(req).await? {
                        Ok((_, token)) => {
                            self.grants
                                .revoke(token.field("token"), is_token_surrogate)
                                .await
                        }
                        Err(refused) => Ok(refused),
                    };
                }
                DEVICE_TOKEN_PATH if post => return self.device_token(req, next).await,
                _ => {}
            }
        }
        if api {
            return self
                .grants
                .forward_api(to, req, injected, next, is_access_surrogate, sign_in_again)
                .await;
        }
        // Fail closed: anything else on an owned host passes the backstop.
        oauth::forward_auth_host(req, next).await
    }

    async fn token(
        &self,
        req: Request<ResponseBody>,
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let (parts, mut token) = match TokenRequest::read(req).await? {
            Ok(read) => read,
            Err(refused) => return Ok(refused),
        };
        match token.field("grant_type") {
            Some("authorization_code") => {
                if !oauth::swap_code(
                    &mut token,
                    &self.codes,
                    ServiceId::Openai,
                    None,
                    Some(DEVICE_REDIRECT),
                ) {
                    return Ok(oauth::token_error(StatusCode::BAD_REQUEST, "invalid_grant"));
                }
                self.exchange(token.into_request(parts, TOKEN_PATH)?, next)
                    .await
            }
            Some("refresh_token") => {
                self.grants
                    .relay_refresh(token, is_refresh_surrogate, invalidated)
                    .await
            }
            // Also the API-key exchange (token exchange): never forwarded.
            _ => Ok(oauth::token_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
            )),
        }
    }

    /// Forward a code exchange; keep the real tokens, answer with
    /// surrogates.
    async fn exchange(
        &self,
        req: Request<ResponseBody>,
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let (parts, bytes) = oauth::forward_buffered(req, next).await?;
        if !parts.status.is_success() {
            return oauth::backstop(oauth::rebuilt(parts, bytes)).await;
        }
        let Some(mut answer) = oauth::answer_object(&parts, &bytes) else {
            return Ok(oauth::server_error());
        };
        let mut take = |name: &str| match answer.remove(name) {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        let (Some(access), Some(refresh), Some(id_token)) = (
            take("access_token"),
            take("refresh_token"),
            take("id_token"),
        ) else {
            return Ok(oauth::server_error());
        };
        if oauth::carries_token(&Value::Object(answer.clone()), oauth::TOKEN_FORMATS) {
            return Ok(oauth::server_error());
        }
        let claims = oauth::jwt_claims(&id_token);
        let account_id = match claims.as_ref().and_then(account_id) {
            Some(id) => id,
            None => oauth::random_account_id()?,
        };
        let surrogates = Surrogates {
            access: access_surrogate(&access)?,
            previous_access: vec![],
            refresh: Some(oauth::surrogate(REFRESH_PREFIX)?),
            id_token: remint(&id_token)?,
        };
        let scopes = answer
            .get("scope")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();
        let grant = self
            .grants
            .insert_grant(NewGrant {
                service: ServiceId::Openai,
                account_id,
                account_label: claims.as_ref().and_then(email),
                secrets: GrantSecrets {
                    access_expires_at: access_expiry(&access),
                    access_token: access,
                    refresh_token: Some(refresh),
                    id_token: Some(id_token),
                    scopes,
                    surrogates,
                    api_keys: vec![],
                },
            })
            .await?;
        tracing::debug!("openai: stored a new sign-in");
        let answer = insert_surrogates(answer, &grant);
        Ok(oauth::rebuilt(
            parts,
            Bytes::from(Value::Object(answer).to_string()),
        ))
    }

    /// The device-code poll: its `authorization_code` reaches the guest as
    /// a surrogate code.
    async fn device_token(
        &self,
        req: Request<ResponseBody>,
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let (parts, bytes) = oauth::forward_buffered(req, next).await?;
        if !parts.status.is_success() {
            return oauth::backstop(oauth::rebuilt(parts, bytes)).await;
        }
        let Some(mut answer) = oauth::answer_object(&parts, &bytes) else {
            return Ok(oauth::server_error());
        };
        let Some(Value::String(code)) = answer.get("authorization_code") else {
            return Ok(oauth::server_error());
        };
        let surrogate = self.codes.issue(code, ServiceId::Openai, Channel::Device)?;
        answer.insert("authorization_code".into(), surrogate.into());
        // The PKCE verifier rides with the surrogate code: without the real
        // code it is worth nothing. Nothing else may carry a secret.
        let mut rest = answer.clone();
        rest.remove("code_verifier");
        if oauth::carries_token(&Value::Object(rest), oauth::TOKEN_FORMATS) {
            return Ok(oauth::server_error());
        }
        Ok(oauth::rebuilt(
            parts,
            Bytes::from(Value::Object(answer).to_string()),
        ))
    }
}

impl Interceptor for Openai {
    fn name(&self) -> &str {
        ServiceId::Openai.name()
    }

    fn targets(&self) -> &[NetworkTarget] {
        &self.targets
    }

    fn send<'a>(
        &'a self,
        to: &'a Endpoint,
        req: Request<ResponseBody>,
        injected: &'a [InjectedSecret],
        next: Next,
    ) -> LocalBoxFuture<'a, anyhow::Result<Response<ResponseBody>>> {
        Box::pin(self.handle(to, req, injected, next))
    }
}

/// The surrogate of a real access token: a fake JWT with the real one's
/// own `exp` claim, else (no `exp`, or not a JWT) a random token.
fn access_surrogate(real: &str) -> anyhow::Result<String> {
    match remint(real)? {
        Some(jwt) => Ok(jwt),
        None => oauth::surrogate(ACCESS_PREFIX),
    }
}

/// A fake JWT surrogate of `real` with `real`'s own `exp` claim. `None`:
/// `real` is not a JWT, or has no `exp`.
fn remint(real: &str) -> anyhow::Result<Option<String>> {
    let Some(exp) = oauth::jwt_claims(real).and_then(|c| c.get("exp")?.as_i64()) else {
        return Ok(None);
    };
    oauth::fake_jwt(real, exp)
}

/// When a real access token expires: its JWT `exp`, else an hour from now.
fn access_expiry(token: &str) -> i64 {
    oauth::jwt_claims(token)
        .and_then(|c| c.get("exp").and_then(Value::as_i64))
        .map_or_else(|| now_ms() + DEFAULT_ACCESS_LIFETIME_MS, |exp| exp * 1000)
}

/// The ChatGPT account of ID-token claims (else the subject).
fn account_id(claims: &Value) -> Option<String> {
    claims
        .get("https://api.openai.com/auth")
        .and_then(|a| a.get("chatgpt_account_id"))
        .or_else(|| claims.get("sub"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(String::from)
}

/// The email address of ID-token claims.
fn email(claims: &Value) -> Option<String> {
    claims
        .get("email")
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|p| p.get("email"))
        })
        .and_then(Value::as_str)
        .map(String::from)
}

/// Put the grant's surrogates into a token answer, leaving `expires_in`
/// (the upstream's own) as it is.
fn insert_surrogates(mut answer: Map<String, Value>, grant: &Grant) -> Map<String, Value> {
    let s = &grant.secrets.surrogates;
    answer.insert("access_token".into(), s.access.clone().into());
    for (field, value) in [("refresh_token", &s.refresh), ("id_token", &s.id_token)] {
        match value {
            Some(v) => answer.insert(field.into(), v.clone().into()),
            None => answer.remove(field),
        };
    }
    answer
}

/// The answer to a refresh whose surrogate airlock does not know (signed
/// out, never issued, or no refresh token).
fn invalidated() -> Response<ResponseBody> {
    oauth::token_error(StatusCode::UNAUTHORIZED, "refresh_token_invalidated")
}

/// The answer to an API request whose sign-in is unknown.
fn sign_in_again() -> Response<ResponseBody> {
    oauth::json_response(
        StatusCode::UNAUTHORIZED,
        &json!({
            "error": {
                "code": "refresh_token_invalidated",
                "message": "airlock: the sign-in has expired; run `codex login` again",
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::store::Surrogates;

    fn secrets() -> GrantSecrets {
        GrantSecrets {
            access_token: "real-access".into(),
            access_expires_at: 1234,
            refresh_token: Some("real-refresh".into()),
            id_token: None,
            scopes: vec![],
            surrogates: Surrogates {
                access: "airlock-at-x".into(),
                previous_access: vec![],
                refresh: Some("airlock-rt-x".into()),
                id_token: Some("old".into()),
            },
            api_keys: vec![],
        }
    }

    /// A refresh answer without an access token keeps the old token and
    /// its expiry; one without an ID token keeps its surrogate.
    #[test]
    fn a_partial_refresh_answer_keeps_the_rest() {
        let answer = json!({ "refresh_token": "new-refresh" });
        let mut got = secrets();
        PROVIDER
            .apply_refresh(&mut got, answer.as_object().unwrap())
            .unwrap();
        assert_eq!(got.access_token, "real-access");
        assert_eq!(got.access_expires_at, 1234);
        assert_eq!(got.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(got.surrogates.id_token.as_deref(), Some("old"));
        assert_eq!(got.surrogates.access, "airlock-at-x");
    }
}
