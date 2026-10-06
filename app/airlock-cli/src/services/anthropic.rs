//! The `anthropic` service: Claude Code's sign-ins (`claude /login` with
//! a Claude.ai subscription or an Anthropic Console account, `claude
//! setup-token`) and its API use.
//!
//! On the token host (`platform.claude.com`) only these routes are served,
//! matched on the normalized path (see [`oauth::normalize_path`]); every
//! other route gets a local `403` ([`oauth::route_not_allowed`]):
//!
//! - `POST /v1/oauth/token` (parsed strictly, see [`oauth`]):
//!   - `authorization_code`: the `code` must be a surrogate code
//!     ([`super::auth_codes`]) issued to this service through the
//!     loopback callback on the port of the `redirect_uri`; it is swapped
//!     for the real code and the exchange forwarded with the guest's
//!     `client_id` (Claude Code has several OAuth clients). The answer's
//!     real tokens are stored as a new grant with that client id and the
//!     guest gets surrogates (access `sk-ant-oat01-airlock-…`, refresh
//!     `sk-ant-ort01-airlock-…`) with the upstream's own `expires_in`.
//!   - `refresh_token`: Claude Code's own refresh, relayed
//!     ([`oauth::Grants::relay_refresh`]) as a refresh of the grant's
//!     scopes without `org:create_api_key`, with the grant's client id; an
//!     unknown refresh surrogate gets `invalid_grant`.
//!   - other grant types: refused locally (`unsupported_grant_type`).
//! - `POST /v1/oauth/token/revoke` (Claude's `/logout`): a refresh or
//!   access surrogate deletes its grant and revokes the real refresh
//!   token upstream (the access token when there is none; Claude Code
//!   never revokes access tokens); the guest gets `200 {}`.
//! - `GET /v1/oauth/hello`: Claude Code's connection check before a
//!   sign-in; forwarded with that path and no query, its answer passes
//!   [`oauth::backstop`].
//!
//! On the API host (`api.anthropic.com`), every path, with strict
//! credentials and the credential swap of [`oauth::Grants::swap_headers`]
//! (`Authorization: Bearer` and `x-api-key` only); a 401 from upstream
//! passes through unchanged (Claude Code refreshes itself). Answers
//! stream through [`super::scan::scan_answer`]. `POST
//! /api/oauth/claude_cli/create_api_key`: every `sk-ant-api…` string of
//! the answer is replaced by a surrogate stored with the grant; a grant
//! creates at most three keys an hour, then the guest gets a local `429`.
//!
//! Any other host (the service owns none) passes [`oauth::backstop`].
//!
//! The access and refresh surrogates stay the same for the life of a
//! grant: they are opaque, so a refresh only changes the real tokens
//! behind them.
//!
//! Known limits:
//!
//! - The manual sign-in (Claude prints a page, the user pastes the code
//!   from `https://platform.claude.com/oauth/code/callback`) brings the
//!   real authorization code into the sandbox through the clipboard;
//!   airlock cannot swap it. Its exchange (exactly that `redirect_uri`) is
//!   forwarded with the real code only when its PKCE verifier belongs to
//!   a sign-in page the browser bridge opened in this process (Claude
//!   uses one verifier for both of its pages), once, within ten minutes
//!   (see [`super::auth_codes`]); else `invalid_grant`. Without the
//!   browser bridge, the manual sign-in does not work.
//! - Claude Code also talks to `mcp-proxy.anthropic.com` at run time. It is
//!   not verified which credential it sends there; the service does not
//!   own that host, so a surrogate sent there stays a surrogate.
//!
//! Protocol facts: Claude Code 2.1.288.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::LocalBoxFuture;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};

use super::auth_codes::PendingCodes;
use super::oauth::{self, Account, Credential, Grants, Provider, TokenRequest, Upstream};
use super::sign_in::SignInPage;
use super::store::{self, ApiKey, Grant, TokenStore};
use super::tokens::{self, Format, Formats, TokenKind};
use super::{ServiceId, scan};
use crate::network::http::ResponseBody;
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::{Endpoint, InjectedSecret, NetworkTarget};

const TOKEN_PATH: &str = "/v1/oauth/token";
const REVOKE_PATH: &str = "/v1/oauth/token/revoke";
const HELLO_PATH: &str = "/v1/oauth/hello";
const CREATE_API_KEY_PATH: &str = "/api/oauth/claude_cli/create_api_key";

/// The `redirect_uri` of the manual sign-in.
const MANUAL_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";

const ACCESS_PREFIX: &str = "sk-ant-oat01-airlock-";
const REFRESH_PREFIX: &str = "sk-ant-ort01-airlock-";
const API_KEY_PREFIX: &str = "sk-ant-api03-airlock-";
const ID_PREFIX: &str = "airlock-id-";

/// The scope a refresh never asks for: Claude Code refreshes without it.
const NOT_REFRESHED_SCOPE: &str = "org:create_api_key";

/// Claude Code listens for the callback on an ephemeral port of the guest
/// (Linux: 32768–60999).
const CALLBACK_PORTS: &[std::ops::RangeInclusive<u16>] = &[32768..=60999];

/// The pages a sign-in callback may send the browser to: the authorize
/// hosts and the success pages (`platform.claude.com/oauth/code/success`,
/// the Console's `buy_credits`), and the Claude.ai origin.
const PAGES: &[&str] = &["platform.claude.com", "claude.com", "claude.ai"];

/// The shortest random part of a real Anthropic token in the scan of
/// API answers.
const REAL_SHAPE_MIN: usize = 80;

/// A real token's shape: `sk-ant-<kind><two digits>-` and at least
/// [`REAL_SHAPE_MIN`] base64url characters.
fn is_real_shape(run: &str) -> bool {
    let b = run.as_bytes();
    b.len() > 13
        && b[10].is_ascii_digit()
        && b[11].is_ascii_digit()
        && b[12] == b'-'
        && b[13..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            .count()
            >= REAL_SHAPE_MIN
}

/// Anthropic's token formats: OAuth access and refresh tokens and API
/// keys, each by its prefix; then, by key, any other value of a token
/// answer's `access_token`, `refresh_token` or `id_token` (the surrogate
/// keeps the prefix Claude Code checks).
pub static FORMATS: Formats = Formats(&[
    Format {
        kind: TokenKind::Access,
        recognize: |_, v| v.starts_with("sk-ant-oat"),
        starts: &["sk-ant-oat"],
        shape: is_real_shape,
        is_surrogate: |v| v.starts_with(ACCESS_PREFIX),
        mint: |_| tokens::surrogate(ACCESS_PREFIX),
        carries_claims: false,
    },
    Format {
        kind: TokenKind::Refresh,
        recognize: |_, v| v.starts_with("sk-ant-ort"),
        starts: &["sk-ant-ort"],
        shape: is_real_shape,
        is_surrogate: |v| v.starts_with(REFRESH_PREFIX),
        mint: |_| tokens::surrogate(REFRESH_PREFIX),
        carries_claims: false,
    },
    Format {
        kind: TokenKind::ApiKey,
        recognize: |_, v| v.starts_with("sk-ant-api"),
        starts: &["sk-ant-api"],
        shape: is_real_shape,
        is_surrogate: |v| v.starts_with(API_KEY_PREFIX),
        mint: |_| tokens::surrogate(API_KEY_PREFIX),
        carries_claims: false,
    },
    // By key: the answer's own token fields, whatever their format.
    Format {
        kind: TokenKind::Access,
        recognize: |k, v| k == "access_token" && !v.is_empty(),
        starts: &[],
        shape: |_| false,
        is_surrogate: |v| v.starts_with(ACCESS_PREFIX),
        mint: |_| tokens::surrogate(ACCESS_PREFIX),
        carries_claims: false,
    },
    Format {
        kind: TokenKind::Refresh,
        recognize: |k, v| k == "refresh_token" && !v.is_empty(),
        starts: &[],
        shape: |_| false,
        is_surrogate: |v| v.starts_with(REFRESH_PREFIX),
        mint: |_| tokens::surrogate(REFRESH_PREFIX),
        carries_claims: false,
    },
    Format {
        kind: TokenKind::Id,
        recognize: |k, v| k == "id_token" && !v.is_empty(),
        starts: &[],
        shape: |_| false,
        is_surrogate: |v| v.starts_with(ID_PREFIX),
        mint: |_| tokens::surrogate(ID_PREFIX),
        carries_claims: false,
    },
]);

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
            callback_ports: CALLBACK_PORTS,
            callback_path: "/callback",
            pages: PAGES,
        },
        SignInPage {
            host: "claude.com",
            path: "/cai/oauth/authorize",
            callback_ports: CALLBACK_PORTS,
            callback_path: "/callback",
            pages: PAGES,
        },
    ]
}

/// Exchange, refresh and revoke at Anthropic.
struct AnthropicOauth;

static PROVIDER: AnthropicOauth = AnthropicOauth;

impl Provider for AnthropicOauth {
    fn id(&self) -> ServiceId {
        ServiceId::Anthropic
    }

    fn formats(&self) -> &'static Formats {
        &FORMATS
    }

    fn token_path(&self) -> &'static str {
        TOKEN_PATH
    }

    fn revoke_path(&self) -> &'static str {
        REVOKE_PATH
    }

    /// `setup-token` issues no refresh token.
    fn exchange_requires(&self) -> &'static [TokenKind] {
        &[TokenKind::Access]
    }

    fn account(&self, answer: &Map<String, Value>) -> Account {
        let field = |object: &str, name: &str| {
            answer
                .get(object)
                .and_then(|o| o.get(name))
                .and_then(Value::as_str)
                .map(String::from)
        };
        Account {
            id: field("account", "uuid"),
            email: field("account", "email_address"),
            organization: field("organization", "name"),
        }
    }

    /// The grant's scopes without [`NOT_REFRESHED_SCOPE`] (no `scope` when
    /// none is left), with the grant's client id.
    fn refresh_body(&self, grant: &Grant, refresh_token: &str) -> Value {
        let mut body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": grant.client_id,
        });
        let scope: Vec<&str> = grant
            .scopes
            .iter()
            .map(String::as_str)
            .filter(|s| *s != NOT_REFRESHED_SCOPE)
            .collect();
        if !scope.is_empty() {
            body["scope"] = scope.join(" ").into();
        }
        body
    }

    /// Claude Code revokes its refresh token only; an access-token revoke
    /// is not known to work.
    fn revokes_access_tokens(&self) -> bool {
        false
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
        let method = req.method().clone();
        if *to == self.endpoints.token {
            match (&method, path.as_str()) {
                (&Method::POST, TOKEN_PATH) => return self.token(req, next).await,
                (&Method::POST, REVOKE_PATH) => {
                    return match TokenRequest::read(req).await? {
                        Ok((_, token)) => self.grants.revoke(token.field("token")).await,
                        Err(refused) => Ok(refused),
                    };
                }
                (&Method::GET, HELLO_PATH) => {
                    oauth::route_to(&mut req, HELLO_PATH)?;
                    return oauth::forward_auth_host(req, next, &FORMATS, &[]).await;
                }
                _ => {}
            }
        }
        if *to == self.endpoints.api {
            if method == Method::POST && path == CREATE_API_KEY_PATH {
                return self.create_api_key(to, req, injected, next).await;
            }
            return self
                .grants
                .forward_api(to, req, injected, next, sign_in_again)
                .await;
        }
        // Fail closed: the token host serves its routes only; a host the
        // service does not know passes the backstop.
        if *to == self.endpoints.token {
            return Ok(oauth::route_not_allowed(ServiceId::Anthropic, to, &req));
        }
        oauth::forward_auth_host(req, next, &FORMATS, &[]).await
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
                if let Err(why) = oauth::swap_code(
                    &mut token,
                    &self.codes,
                    ServiceId::Anthropic,
                    Some(MANUAL_REDIRECT),
                    None,
                ) {
                    return Ok(oauth::token_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        format_args!("anthropic: {why}"),
                    ));
                }
                self.grants.exchange(parts, token, next).await
            }
            Some("refresh_token") => self.grants.relay_refresh(token, invalid_grant).await,
            other => Ok(oauth::token_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                format_args!("anthropic: a token request with grant_type {other:?}"),
            )),
        }
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
        let swapped = match self
            .grants
            .swap_headers(to, req.headers_mut(), injected, sign_in_again)
            .await?
        {
            Ok(swapped) => swapped,
            Err(refused) => return Ok(refused),
        };
        let known = self.grants.known_reals(&swapped, injected).await?;
        let Credential::Grant(grant_id) = swapped.credential else {
            // An injected token's key is the user's own: the answer still
            // passes the scan, which refuses a real key.
            let (parts, bytes) = oauth::forward_buffered(req, next).await?;
            return scan::scan_answer(oauth::rebuilt(parts, bytes), &FORMATS, known).await;
        };
        if !self
            .store()
            .take_api_key_slot(ServiceId::Anthropic, &grant_id)
            .await?
        {
            return Ok(too_many_api_keys());
        }
        let (parts, bytes) = oauth::forward_buffered(req, next).await?;
        if !parts.status.is_success() {
            return scan::scan_answer(oauth::rebuilt(parts, bytes), &FORMATS, known).await;
        }
        let Some(answer) = oauth::answer_object(&parts, &bytes) else {
            return Ok(oauth::server_error(
                "anthropic: a create_api_key answer that is no uncompressed JSON object",
            ));
        };
        // The answer's other fields are not known: only the key formats
        // count, and an OAuth token in it refuses it.
        let found = match tokens::collect(&FORMATS, &answer, &[TokenKind::ApiKey], false) {
            Ok(found) if !found.is_empty() => found,
            Ok(_) => {
                return Ok(oauth::server_error(
                    "anthropic: a create_api_key answer without an API key",
                ));
            }
            Err(refusal) => {
                return Ok(oauth::server_error(format_args!(
                    "anthropic: a create_api_key answer: {refusal}"
                )));
            }
        };
        let keys: Vec<ApiKey> = tokens::mint_all(&found)?
            .into_iter()
            .map(|t| ApiKey {
                real: t.real,
                surrogate: t.surrogate,
            })
            .collect();
        let surrogates = keys
            .iter()
            .map(|k| (k.real.clone(), k.surrogate.clone()))
            .collect();
        self.store()
            .add_api_keys(ServiceId::Anthropic, &grant_id, keys)
            .await?;
        let mut answer = Value::Object(answer);
        tokens::substitute(&mut answer, &surrogates);
        Ok(oauth::rebuilt(parts, Bytes::from(answer.to_string())))
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

/// The answer to a `create_api_key` beyond the limit of the grant.
fn too_many_api_keys() -> Response<ResponseBody> {
    tracing::warn!(
        "refused: anthropic: a create_api_key beyond {} keys an hour for the sign-in (answered \
         429)",
        store::API_KEY_CREATIONS
    );
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
    oauth::token_error(
        StatusCode::BAD_REQUEST,
        "invalid_grant",
        "anthropic: a refresh with a refresh token that is no surrogate of a stored sign-in",
    )
}

/// The answer to an API request whose sign-in is unknown.
fn sign_in_again() -> Response<ResponseBody> {
    tracing::warn!(
        "refused: anthropic: an API request with a surrogate of no stored sign-in (answered 401)"
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(scopes: &[&str]) -> Grant {
        let mut grant = Grant::for_tests(vec![]);
        grant.client_id = "stored-client".into();
        grant.scopes = scopes.iter().map(ToString::to_string).collect();
        grant
    }

    /// A refresh asks for the grant's scopes without
    /// `org:create_api_key`, with the grant's client id; a grant with no
    /// other scope asks for none.
    #[test]
    fn the_refresh_body_is_claude_codes() {
        let body = PROVIDER.refresh_body(
            &grant(&["org:create_api_key", "user:inference", "user:profile"]),
            "real-refresh",
        );
        assert_eq!(
            body,
            json!({
                "grant_type": "refresh_token",
                "refresh_token": "real-refresh",
                "client_id": "stored-client",
                "scope": "user:inference user:profile",
            })
        );
        let body = PROVIDER.refresh_body(&grant(&["org:create_api_key"]), "real-refresh");
        assert!(body.get("scope").is_none(), "{body}");
    }

    #[test]
    fn real_tokens_are_told_from_surrogates() {
        for real in ["sk-ant-api03-x", "sk-ant-oat01-x", "sk-ant-ort01-x"] {
            assert!(FORMATS.is_real(real), "{real}");
        }
        for other in [
            "sk-ant-api03-airlock-x",
            "sk-ant-oat01-airlock-x",
            "sk-ant-ort01-airlock-x",
            "rt_abcdefghijklmnopqrstuvwxyz0123456789",
            "x",
        ] {
            assert!(!FORMATS.is_real(other), "{other}");
        }
    }

    /// Surrogates have their prefix and 48 random bytes (64 base64url
    /// characters) after it.
    #[test]
    fn surrogates_have_enough_entropy() {
        for format in FORMATS.0 {
            let s = (format.mint)("sk-ant-oat01-x").unwrap();
            assert!((format.is_surrogate)(&s), "{s}");
            // 48 random bytes, 64 base64url characters, after the prefix.
            assert!(s.len() >= ID_PREFIX.len() + 64, "{s}");
        }
    }
}
