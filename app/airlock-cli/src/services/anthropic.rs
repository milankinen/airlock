//! The `anthropic` service: Claude Code's sign-in (`claude /login`,
//! `claude setup-token`) and its API use.
//!
//! On the token host (`platform.claude.com`), matched on the normalized
//! path (see [`oauth::normalize_path`]):
//!
//! - `POST /v1/oauth/token` (parsed strictly, see [`oauth`]):
//!   - `authorization_code`: the `code` must be a surrogate code
//!     ([`super::auth_codes`]) issued to this service through the
//!     loopback callback on the port of the `redirect_uri`; it is swapped
//!     for the real code and the exchange forwarded. The answer's real
//!     tokens are stored as a new grant and the guest gets surrogates
//!     (access `sk-ant-oat01-airlock-…`, refresh `sk-ant-ort01-airlock-…`)
//!     with the upstream's own `expires_in`.
//!   - `refresh_token`: Claude Code's own refresh, relayed
//!     ([`oauth::Grants::relay_refresh`]); an unknown refresh surrogate
//!     gets `invalid_grant`.
//!   - other grant types: refused locally (`unsupported_grant_type`).
//! - `POST /v1/oauth/token/revoke` (Claude's `/logout`): a refresh or
//!   access surrogate deletes its grant and revokes the real refresh
//!   token upstream (the access token when there is none; Claude Code
//!   never revokes access tokens); the guest gets `200 {}`.
//! - Everything else passes [`oauth::backstop`], without a token swap.
//!
//! On the API host (`api.anthropic.com`), every path, with strict
//! credentials (see [`super`]): `Authorization: Bearer <surrogate>` and
//! `x-api-key: <surrogate>` get the real values, an injected masked
//! secret passes, anything else gets a local `401`; a 401 from upstream
//! passes through unchanged (Claude Code refreshes itself). Answers pass
//! [`oauth::api_backstop`]. `POST /api/oauth/claude_cli/create_api_key`:
//! every `sk-ant-api…` string of the answer is replaced by a surrogate
//! stored with the grant; a grant creates at most three keys an hour,
//! then the guest gets a local `429`.
//!
//! Any other host (the service owns none) passes [`oauth::backstop`].
//!
//! Both the access and the refresh surrogate stay the same for the life
//! of a grant: Claude Code's own refresh never changes the real access
//! token's format in a way that needs a new one.
//!
//! Known limits:
//!
//! - The manual sign-in (Claude prints a page, the user pastes the code
//!   from `https://platform.claude.com/oauth/code/callback`) brings the
//!   real authorization code into the sandbox through the clipboard;
//!   airlock cannot swap it. Its exchange (exactly that `redirect_uri`) is
//!   forwarded with the real code.
//! - Claude Code also talks to `mcp-proxy.anthropic.com` at run time. It is
//!   not verified which credential it sends there; the service does not
//!   own that host, so a surrogate sent there stays a surrogate.
//!
//! Protocol facts: Claude Code 2.1.288.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::LocalBoxFuture;
use hyper::header::HeaderValue;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};

use super::ServiceId;
use super::auth_codes::PendingCodes;
use super::oauth::{self, Credential, Grants, Provider, TokenRequest, Upstream};
use super::sign_in::SignInPage;
use super::store::{
    self, ApiKey, Grant, GrantSecrets, NewGrant, SurrogateKind, Surrogates, TokenStore, now_ms,
};
use crate::network::http::ResponseBody;
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::{Endpoint, InjectedSecret, NetworkTarget};

/// Claude Code's OAuth client id.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

const TOKEN_PATH: &str = "/v1/oauth/token";
const REVOKE_PATH: &str = "/v1/oauth/token/revoke";
const CREATE_API_KEY_PATH: &str = "/api/oauth/claude_cli/create_api_key";

/// The `redirect_uri` of the manual sign-in.
const MANUAL_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";

const ACCESS_PREFIX: &str = "sk-ant-oat01-airlock-";
const REFRESH_PREFIX: &str = "sk-ant-ort01-airlock-";
const API_KEY_PREFIX: &str = "sk-ant-api03-airlock-";
/// What a real API key in a `create_api_key` answer starts with.
const REAL_API_KEY_PREFIX: &str = "sk-ant-api";

/// The scopes Claude Code asks for: `/login` asks for all, `setup-token`
/// for `user:inference` only.
const SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
    "user:plugins",
];

/// Claude Code listens for the callback on an ephemeral port of the guest
/// (Linux: 32768–60999).
const CALLBACK_PORTS: &[std::ops::RangeInclusive<u16>] = &[32768..=60999];

/// The pages a sign-in callback may send the browser to: the authorize
/// hosts and the success pages (`platform.claude.com/oauth/code/success`,
/// the Console's `buy_credits`), and the Claude.ai origin.
const PAGES: &[&str] = &["platform.claude.com", "claude.com", "claude.ai"];

/// Where the service's hosts are.
pub struct Endpoints {
    /// Token exchange, refresh and revoke (`platform.claude.com`).
    pub token: Endpoint,
    /// The API (`api.anthropic.com`).
    pub api: Endpoint,
}

impl Endpoints {
    pub fn production() -> Self {
        Self {
            token: Endpoint::new("platform.claude.com", 443),
            api: Endpoint::new("api.anthropic.com", 443),
        }
    }

    pub fn targets(&self) -> Vec<NetworkTarget> {
        crate::network::target::targets_of(&[&self.token, &self.api])
    }
}

/// The sign-in pages: the Console and the Claude.ai subscription login.
pub fn sign_in_pages() -> Vec<SignInPage> {
    vec![
        SignInPage {
            host: "platform.claude.com",
            path: "/oauth/authorize",
            client_id: CLIENT_ID,
            scopes: SCOPES,
            callback_ports: CALLBACK_PORTS,
            callback_path: "/callback",
            pages: PAGES,
        },
        SignInPage {
            host: "claude.com",
            path: "/cai/oauth/authorize",
            client_id: CLIENT_ID,
            scopes: SCOPES,
            callback_ports: CALLBACK_PORTS,
            callback_path: "/callback",
            pages: PAGES,
        },
    ]
}

fn is_access_surrogate(s: &str) -> bool {
    s.starts_with(ACCESS_PREFIX)
}

fn is_refresh_surrogate(s: &str) -> bool {
    s.starts_with(REFRESH_PREFIX)
}

fn is_token_surrogate(s: &str) -> bool {
    is_access_surrogate(s) || is_refresh_surrogate(s)
}

/// Refresh and revoke at Anthropic.
struct AnthropicOauth;

static PROVIDER: AnthropicOauth = AnthropicOauth;

impl Provider for AnthropicOauth {
    fn id(&self) -> ServiceId {
        ServiceId::Anthropic
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

    /// Claude Code revokes its refresh token only; an access-token revoke
    /// is not known to work.
    fn revokes_access_tokens(&self) -> bool {
        false
    }

    /// The access surrogate stays the same: Claude Code's real access
    /// token is an opaque string, not a JWT airlock re-mints.
    fn apply_refresh(
        &self,
        secrets: &mut GrantSecrets,
        answer: &Map<String, Value>,
    ) -> anyhow::Result<()> {
        let Some(access) = answer.get("access_token").and_then(Value::as_str) else {
            anyhow::bail!("token refresh answered no access token");
        };
        secrets.access_token = access.to_string();
        let expires_in = answer
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600);
        secrets.access_expires_at = now_ms() + expires_in * 1000;
        if let Some(refresh) = answer.get("refresh_token").and_then(Value::as_str) {
            secrets.refresh_token = Some(refresh.to_string());
        }
        let scopes = scopes_of(answer);
        if !scopes.is_empty() {
            secrets.scopes = scopes;
        }
        Ok(())
    }

    fn surrogate_answer(&self, grant: &Grant, answer: Map<String, Value>) -> Map<String, Value> {
        insert_surrogates(answer, grant)
    }
}

pub struct Anthropic {
    endpoints: Endpoints,
    grants: Grants,
    codes: PendingCodes,
    targets: Vec<NetworkTarget>,
}

impl Anthropic {
    pub fn new(
        endpoints: Endpoints,
        store: Arc<TokenStore>,
        tls: Arc<rustls::ClientConfig>,
        codes: PendingCodes,
    ) -> Self {
        Self {
            grants: Grants::new(
                &PROVIDER,
                store,
                Upstream::new(tls, endpoints.token.clone()),
            ),
            codes,
            targets: endpoints.targets(),
            endpoints,
        }
    }

    fn store(&self) -> &TokenStore {
        &self.grants.store
    }

    async fn handle(
        &self,
        to: &Endpoint,
        mut req: Request<ResponseBody>,
        injected: &[InjectedSecret],
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        oauth::pin_authority(&mut req, to)?;
        let path = oauth::normalize_path(req.uri().path());
        let post = req.method() == Method::POST;
        if *to == self.endpoints.token {
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
                _ => {}
            }
        }
        // Fail closed: only the API host gets a swap; anything else on an
        // owned host passes the backstop.
        if *to != self.endpoints.api {
            return oauth::forward_auth_host(req, next).await;
        }
        if let Err(refused) = self.swap_api_key(to, &mut req, injected).await? {
            return Ok(refused);
        }
        if post && path == CREATE_API_KEY_PATH {
            return self.create_api_key(to, req, injected, next).await;
        }
        self.grants
            .forward_api(to, req, injected, next, is_access_surrogate, sign_in_again)
            .await
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
                    ServiceId::Anthropic,
                    Some(MANUAL_REDIRECT),
                    None,
                ) {
                    return Ok(oauth::token_error(StatusCode::BAD_REQUEST, "invalid_grant"));
                }
                self.exchange(token.into_request(parts, TOKEN_PATH)?, next)
                    .await
            }
            Some("refresh_token") => {
                self.grants
                    .relay_refresh(token, is_refresh_surrogate, invalid_grant)
                    .await
            }
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
        let Some(Value::String(access)) = answer.remove("access_token") else {
            return Ok(oauth::server_error());
        };
        let refresh = match answer.remove("refresh_token") {
            None => None,
            Some(Value::String(refresh)) => Some(refresh),
            Some(_) => return Ok(oauth::server_error()),
        };
        answer.remove("refresh_token_expires_in");
        if oauth::carries_token(&Value::Object(answer.clone()), oauth::TOKEN_FORMATS) {
            return Ok(oauth::server_error());
        }
        let expires_in = answer
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600);
        let scopes = scopes_of(&answer);
        let account = answer.get("account");
        let account_id = match account.and_then(|a| a.get("uuid")).and_then(Value::as_str) {
            Some(uuid) => uuid.to_string(),
            None => oauth::random_account_id()?,
        };
        let account_label = account
            .and_then(|a| a.get("email_address"))
            .and_then(Value::as_str)
            .map(String::from);
        let surrogates = Surrogates {
            access: oauth::surrogate(ACCESS_PREFIX)?,
            previous_access: vec![],
            refresh: refresh
                .as_ref()
                .map(|_| oauth::surrogate(REFRESH_PREFIX))
                .transpose()?,
            id_token: None,
        };
        let grant = self
            .grants
            .insert_grant(NewGrant {
                service: ServiceId::Anthropic,
                account_id,
                account_label,
                secrets: GrantSecrets {
                    access_token: access,
                    access_expires_at: now_ms() + expires_in * 1000,
                    refresh_token: refresh,
                    id_token: None,
                    scopes,
                    surrogates,
                    api_keys: vec![],
                },
            })
            .await?;
        tracing::debug!("anthropic: stored a new sign-in");
        let answer = PROVIDER.surrogate_answer(&grant, answer);
        Ok(oauth::rebuilt(
            parts,
            Bytes::from(Value::Object(answer).to_string()),
        ))
    }

    /// `POST create_api_key` with a grant's token: the created key reaches
    /// the guest as a surrogate. At most [`store::API_KEY_CREATIONS`] keys
    /// per grant in [`store::API_KEY_WINDOW_MS`]; beyond, a local 429.
    async fn create_api_key(
        &self,
        to: &Endpoint,
        mut req: Request<ResponseBody>,
        injected: &[InjectedSecret],
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let grant = match self
            .grants
            .api_credential(
                to,
                req.headers(),
                injected,
                is_access_surrogate,
                sign_in_again,
            )
            .await?
        {
            Ok(Credential::Grant(grant)) => *grant,
            // An injected token's key is the user's own: the answer still
            // passes the API backstop, which refuses a real key.
            Ok(Credential::None | Credential::Injected) => {
                let (parts, bytes) = oauth::forward_buffered(req, next).await?;
                return oauth::api_backstop(oauth::rebuilt(parts, bytes)).await;
            }
            Err(refused) => return Ok(refused),
        };
        if !self.store().take_api_key_slot(&grant.id).await? {
            return Ok(too_many_api_keys());
        }
        oauth::set_bearer(req.headers_mut(), &grant)?;
        let (parts, bytes) = oauth::forward_buffered(req, next).await?;
        if !parts.status.is_success() {
            return oauth::api_backstop(oauth::rebuilt(parts, bytes)).await;
        }
        let Some(answer) = oauth::answer_object(&parts, &bytes) else {
            return Ok(oauth::server_error());
        };
        let mut answer = Value::Object(answer);
        let mut keys = Vec::new();
        replace_api_keys(&mut answer, &mut keys)?;
        if keys.is_empty() || oauth::carries_token(&answer, &["sk-ant-oat", "sk-ant-ort"]) {
            return Ok(oauth::server_error());
        }
        for key in keys {
            self.store()
                .add_api_key(&grant.id, ServiceId::Anthropic, key)
                .await?;
        }
        Ok(oauth::rebuilt(parts, Bytes::from(answer.to_string())))
    }

    /// The strict check of `x-api-key` (see the module docs), before the
    /// request goes on: an API-key surrogate of a grant gets the real key;
    /// a masked secret airlock injected passes. `Err` holds the local
    /// answer to any other value.
    async fn swap_api_key(
        &self,
        to: &Endpoint,
        req: &mut Request<ResponseBody>,
        injected: &[InjectedSecret],
    ) -> anyhow::Result<Result<(), Response<ResponseBody>>> {
        let Some(value) = req.headers().get("x-api-key") else {
            return Ok(Ok(()));
        };
        let value = value.to_str().unwrap_or_default().to_string();
        if !value.starts_with(API_KEY_PREFIX) {
            return Ok(if oauth::is_injected(&value, injected) {
                Ok(())
            } else {
                Err(oauth::foreign_credential(ServiceId::Anthropic, to))
            });
        }
        let real = self
            .store()
            .find_by_surrogate(ServiceId::Anthropic, SurrogateKind::ApiKey, &value)
            .await?
            .and_then(|grant| {
                grant
                    .secrets
                    .api_keys
                    .into_iter()
                    .find(|k| k.surrogate == value)
            });
        let Some(key) = real else {
            return Ok(Err(sign_in_again()));
        };
        let real = HeaderValue::from_str(&key.real)
            .map_err(|_| anyhow::anyhow!("the stored API key is not a valid header value"))?;
        req.headers_mut().insert("x-api-key", real);
        Ok(Ok(()))
    }
}

impl Interceptor for Anthropic {
    fn name(&self) -> &str {
        ServiceId::Anthropic.name()
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

/// The `scope` of a token answer, split.
fn scopes_of(answer: &Map<String, Value>) -> Vec<String> {
    answer
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Put the grant's surrogates into a token answer, leaving `expires_in`
/// (the upstream's own) as it is.
fn insert_surrogates(mut answer: Map<String, Value>, grant: &Grant) -> Map<String, Value> {
    let s = &grant.secrets.surrogates;
    answer.insert("access_token".into(), s.access.clone().into());
    match &s.refresh {
        Some(refresh) => answer.insert("refresh_token".into(), refresh.clone().into()),
        None => answer.remove("refresh_token"),
    };
    answer
}

/// Replace every real API key string in `value` by a new surrogate; the
/// pairs go to `keys`.
fn replace_api_keys(value: &mut Value, keys: &mut Vec<ApiKey>) -> anyhow::Result<()> {
    match value {
        Value::String(s) if s.starts_with(REAL_API_KEY_PREFIX) => {
            let surrogate = oauth::surrogate(API_KEY_PREFIX)?;
            keys.push(ApiKey {
                real: std::mem::replace(s, surrogate.clone()),
                surrogate,
            });
        }
        Value::Array(items) => {
            for v in items {
                replace_api_keys(v, keys)?;
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                replace_api_keys(v, keys)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The answer to a `create_api_key` beyond the limit of the grant.
fn too_many_api_keys() -> Response<ResponseBody> {
    oauth::json_response(
        StatusCode::TOO_MANY_REQUESTS,
        &json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": format!(
                    "airlock: this sign-in created {} API keys in the last hour; try again later",
                    store::API_KEY_CREATIONS
                ),
            },
        }),
    )
}

/// The answer to a refresh whose surrogate airlock does not know (signed
/// out, never issued, or no refresh token).
fn invalid_grant() -> Response<ResponseBody> {
    oauth::token_error(StatusCode::BAD_REQUEST, "invalid_grant")
}

/// The answer to an API request whose sign-in is unknown.
fn sign_in_again() -> Response<ResponseBody> {
    oauth::json_response(
        StatusCode::UNAUTHORIZED,
        &json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "airlock: the sign-in has expired; sign in again (/login)",
            },
        }),
    )
}
