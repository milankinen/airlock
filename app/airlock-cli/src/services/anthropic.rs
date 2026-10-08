//! The `anthropic` network service.
//!
//! Supports the sign-ins of Claude Code and its API use. The sign-ins are
//! `claude /login` with a Claude.ai subscription or an Anthropic Console
//! account, and `claude setup-token`.
//!
//! The protocol details agree with Claude Code 2.1.288.

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

/// The scope that a refresh never asks for. Claude Code refreshes without
/// it.
const NOT_REFRESHED_SCOPE: &str = "org:create_api_key";

/// Claude Code listens for the callback on an ephemeral port of the guest
/// (Linux: 32768–60999).
const CALLBACK_PORTS: &[std::ops::RangeInclusive<u16>] = &[32768..=60999];

/// Pages that a sign-in callback can send the browser to: the authorize
/// hosts, the success pages (`platform.claude.com/oauth/code/success`,
/// the Console's `buy_credits`), and the Claude.ai origin.
const PAGES: &[&str] = &["platform.claude.com", "claude.com", "claude.ai"];

/// Minimum length of the random part of a real Anthropic token in the
/// scan of API answers.
const REAL_SHAPE_MIN: usize = 80;

/// Whether `run` has the shape of a real token: `sk-ant-<kind><two
/// digits>-` and at least [`REAL_SHAPE_MIN`] base64url characters.
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

/// Anthropic's token formats.
///
/// First the OAuth access and refresh tokens and the API keys, each by its
/// prefix. Then, by key, any other value of the `access_token`,
/// `refresh_token` or `id_token` of a token answer. The surrogate keeps
/// the prefix that Claude Code checks.
///
/// The access and refresh surrogates stay the same for the life of a
/// grant. They are opaque, so a refresh changes only the real tokens.
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

/// The hosts of the service.
pub struct Endpoints {
    /// Token exchange, refresh and revoke (`platform.claude.com`).
    pub token: Endpoint,
    /// The API (`api.anthropic.com`).
    pub api: Endpoint,
}

impl Endpoints {
    /// The production hosts.
    pub fn production() -> Self {
        Self {
            token: Endpoint::new("platform.claude.com", 443),
            api: Endpoint::new("api.anthropic.com", 443),
        }
    }

    /// The network targets of the hosts.
    pub fn targets(&self) -> Vec<NetworkTarget> {
        crate::network::target::targets_of(&[&self.token, &self.api])
    }
}

/// Get the sign-in pages: the Console and the Claude.ai subscription
/// login.
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

    /// Only the access token, because `setup-token` issues no refresh
    /// token.
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

    /// Asks for the grant's scopes without [`NOT_REFRESHED_SCOPE`] (no
    /// `scope` if no scope is left), with the grant's client id.
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

    /// Claude Code revokes only its refresh token. It is not known if an
    /// access-token revoke works.
    fn revokes_access_tokens(&self) -> bool {
        false
    }
}

/// The interceptor of the `anthropic` service.
///
/// On the token host (`platform.claude.com`), only these routes are
/// served. They match on the normalized path ([`oauth::normalize_path`]).
/// Every other route gets a local `403` ([`oauth::route_not_allowed`]).
///  * `POST /v1/oauth/token` (parsed strictly, see [`TokenRequest`]):
///    - `authorization_code`: the `code` must be a surrogate code
///      ([`super::auth_codes`]) issued to this service through the
///      loopback callback on the port of the `redirect_uri`. The proxy
///      puts the real code in its place and forwards the exchange with the
///      guest's `client_id` (Claude Code has several OAuth clients). The
///      real tokens of the answer are stored as a new grant with that
///      client id. The guest gets surrogates (access
///      `sk-ant-oat01-airlock-…`, refresh `sk-ant-ort01-airlock-…`) with
///      the upstream's own `expires_in`.
///    - `refresh_token`: Claude Code's own refresh, relayed
///      ([`Grants::relay_refresh`]). An unknown refresh surrogate gets
///      `invalid_grant`.
///    - Other grant types: refused locally (`unsupported_grant_type`).
///  * `POST /v1/oauth/token/revoke` (Claude's `/logout`): a refresh or
///    access surrogate deletes its grant and revokes the real refresh
///    token upstream (the access token if there is no refresh token). The
///    guest gets `200 {}`.
///  * `GET /v1/oauth/hello`: Claude Code's connection check before a
///    sign-in. Forwarded with that path and no query. Its answer goes
///    through [`oauth::backstop`].
///
/// On the API host (`api.anthropic.com`), every path is served, with the
/// credential swap of [`Grants::swap_headers`]. Answers stream through
/// [`scan::scan_answer`]. A 401 from upstream passes through unchanged
/// (Claude Code refreshes itself). For `create_api_key`, see
/// [`Self::create_api_key`].
///
/// Any other host passes [`oauth::backstop`].
///
/// Known limits:
///  * The manual sign-in brings the real authorization code into the
///    sandbox through the clipboard: Claude shows a page, and the user
///    pastes the code from `https://platform.claude.com/oauth/code/callback`.
///    Airlock cannot replace that code. The proxy forwards its exchange
///    (exactly that `redirect_uri`) with the real code only if its PKCE
///    verifier belongs to a sign-in page that the browser bridge opened in
///    this process. This works once, in ten minutes (see
///    [`super::auth_codes`]). Claude uses one verifier for both of its
///    pages. Otherwise the exchange gets `invalid_grant`. Without the
///    browser bridge, the manual sign-in does not work.
///  * Claude Code also connects to `mcp-proxy.anthropic.com` at run time.
///    It is not verified which credential it sends there. The service
///    does not own that host, so a surrogate sent there stays a surrogate.
pub struct Anthropic {
    endpoints: Endpoints,
    grants: Grants,
    codes: PendingCodes,
    targets: Vec<NetworkTarget>,
}

impl Anthropic {
    /// Make the interceptor.
    /// Args:
    ///  - `endpoints`: The hosts of the service
    ///  - `store`: The shared token store
    ///  - `tls`: TLS config for the proxy's own calls to the token host
    ///  - `codes`: Surrogate codes, shared with the service's sign-ins.
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
        // Fail closed: the token host serves only its routes. A host that
        // the service does not know goes through the backstop.
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

    /// Handle `POST /api/oauth/claude_cli/create_api_key` with a grant's
    /// token.
    ///
    /// Every `sk-ant-api…` string of the answer is replaced with a
    /// surrogate stored with the grant. A grant can create at most
    /// [`store::API_KEY_CREATIONS`] keys in [`store::API_KEY_WINDOW_MS`].
    /// More requests get a local `429`.
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
            // The key of an injected token belongs to the user. The answer
            // still goes through the scan, which refuses a real key.
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
        // The other fields of the answer are not known. Only the key
        // formats count. An OAuth token in the answer refuses the answer.
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

/// Make the answer for a `create_api_key` above the limit of the grant.
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

/// Make the answer for a refresh with a surrogate that airlock does not
/// know (signed out, never issued, or no refresh token).
fn invalid_grant() -> Response<ResponseBody> {
    oauth::token_error(
        StatusCode::BAD_REQUEST,
        "invalid_grant",
        "anthropic: a refresh with a refresh token that is no surrogate of a stored sign-in",
    )
}

/// Make the answer for an API request with an unknown sign-in.
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
    //! Anthropic token formats: real tokens and minted surrogates.

    use super::*;

    /// Test that a token with an Anthropic prefix counts as real, but an
    /// airlock surrogate with the same prefix does not.
    ///   1. Check that API keys, access tokens and refresh tokens are real
    ///   2. Check that surrogates, a token of a different provider and a short
    ///      string are not real
    #[test]
    fn token_with_provider_prefix_is_real_unless_airlock_surrogate() {
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

    /// Test that each format mints a surrogate that it recognizes and that
    /// is a known prefix with 48 random bytes (64 base64url characters).
    ///   1. Mint a surrogate with each format
    ///   2. Check that the format recognizes it
    ///   3. Check that a prefix starts it and that the rest decodes to
    ///      exactly 48 bytes
    #[test]
    fn minted_surrogate_has_prefix_and_48_random_bytes() {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        for format in FORMATS.0 {
            let s = (format.mint)("sk-ant-oat01-x").unwrap();
            assert!((format.is_surrogate)(&s), "{s}");
            let rest = [ACCESS_PREFIX, REFRESH_PREFIX, API_KEY_PREFIX, ID_PREFIX]
                .into_iter()
                .find_map(|prefix| s.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("no known prefix: {s}"));
            let random = URL_SAFE_NO_PAD.decode(rest).unwrap();
            assert_eq!(random.len(), tokens::SURROGATE_BYTES, "{s}");
        }
    }
}
