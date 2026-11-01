//! The `anthropic` network service.
//!
//! Supports the sign-ins of Claude Code and its API use. The sign-ins are
//! `claude /login` with a Claude.ai subscription or an Anthropic Console
//! account, and `claude setup-token`.
//!
//! The protocol details agree with Claude Code 2.1.288. The Remote
//! Control details agree with Claude Code 2.1.312.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use futures::future::LocalBoxFuture;
use hyper::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};

use super::auth_codes::PendingCodes;
use super::oauth::{self, Account, Credential, Grants, Provider, Swapped, TokenRequest, Upstream};
use super::sign_in::SignInPage;
use super::store::{self, ApiKey, Grant, TokenStore, random_bytes};
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

/// Most worker tokens kept for one Remote Control session. Claude Code
/// gets a new token before the old one expires. Streams with the old
/// token can stay open for a short time.
const WORKER_TOKENS_PER_SESSION: usize = 3;

/// Most Remote Control sessions with kept worker tokens. A new session
/// removes the oldest session. A sandbox normally runs one session. Each
/// kept real token makes the scan of each API answer slower.
const WORKER_SESSIONS: usize = 8;

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

/// A Remote Control route on the API host.
#[derive(Clone, Copy, PartialEq)]
enum SessionRoute {
    /// `POST /v1/code/sessions/{id}/bridge`: mints a worker token.
    Bridge,
    /// `/v1/code/sessions/{id}/worker` and the paths below it.
    Worker,
}

/// Get the Remote Control route of a request and its session id.
/// Args:
///  - `raw`: The raw path of the request
///  - `path`: The normalized path ([`oauth::normalize_path`]).
///
/// Returns:
///   The route and the `{id}`, spelled as in `raw`, or `None` for other
///   paths.
fn session_route<'a>(raw: &'a str, path: &str) -> Option<(SessionRoute, &'a str)> {
    // The raw path goes upstream, so it must be the route itself, not a
    // path that only normalizes onto it (`..`, `%2F`). Normalizing
    // lowercases, and session ids have uppercase letters.
    if !raw.eq_ignore_ascii_case(path) {
        return None;
    }
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let route = match segments.as_slice() {
        ["v1", "code", "sessions", _, "bridge"] => SessionRoute::Bridge,
        ["v1", "code", "sessions", _, "worker", ..] => SessionRoute::Worker,
        _ => return None,
    };
    // Same segments as `path`, because only the case is different.
    raw.split('/').nth(4).map(|id| (route, id))
}

/// Get the token of the only credential of `headers`: one
/// `Authorization: Bearer` header and no `x-api-key`.
fn only_bearer(headers: &HeaderMap) -> Option<&str> {
    if headers.contains_key("x-api-key") {
        return None;
    }
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    value.to_str().ok().and_then(oauth::bearer_token)
}

/// The optional prefix of a worker token. Claude Code removes it before it
/// reads the JWT.
const WORKER_TOKEN_PREFIX: &str = "sk-ant-si-";

/// Random bytes of the signature of a worker token surrogate. An ES256
/// signature also has 64 bytes.
const WORKER_SIGNATURE_BYTES: usize = 64;

/// Make a surrogate for the worker token `real`.
///
/// The surrogate keeps the optional `sk-ant-si-` prefix and the header and
/// claims of the JWT, because Claude Code reads the claims (`exp`,
/// `session_id`). Only the signature is random.
/// Returns:
///   The surrogate, or `None` if `real` is not a JWT with three non-empty
///   base64url parts. An error if the random source fails.
fn worker_surrogate(real: &str) -> anyhow::Result<Option<String>> {
    let jwt = real.strip_prefix(WORKER_TOKEN_PREFIX).unwrap_or(real);
    let mut parts = jwt.split('.');
    let (Some(header), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(None);
    };
    let base64url = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    };
    if ![header, claims, signature].into_iter().all(base64url) {
        return Ok(None);
    }
    let prefix = &real[..real.len() - jwt.len()];
    let random = URL_SAFE_NO_PAD.encode(random_bytes::<WORKER_SIGNATURE_BYTES>()?);
    Ok(Some(format!("{prefix}{header}.{claims}.{random}")))
}

/// A worker token: the surrogate that the guest has, and the real token.
struct WorkerToken {
    surrogate: String,
    real: String,
}

/// Worker tokens that Anthropic minted for Remote Control sessions in this
/// process, by session id. The newest session is last.
///
/// A token has no expiry here. The upstream decides the expiry, because
/// heartbeats can extend it.
#[derive(Default)]
struct WorkerTokens(RefCell<VecDeque<(String, VecDeque<WorkerToken>)>>);

impl WorkerTokens {
    /// Keep `token` for `session`. Removes the oldest token of the session
    /// and the oldest session above the limits. A real token that is
    /// already kept gets the new surrogate.
    fn add(&self, session: &str, token: WorkerToken) {
        let mut sessions = self.0.borrow_mut();
        let mut tokens = match sessions.iter().position(|(id, _)| id == session) {
            Some(i) => sessions.remove(i).map(|(_, t)| t).unwrap_or_default(),
            None => VecDeque::new(),
        };
        tokens.retain(|t| t.real != token.real);
        tokens.push_back(token);
        if tokens.len() > WORKER_TOKENS_PER_SESSION {
            tokens.pop_front();
        }
        sessions.push_back((session.to_string(), tokens));
        if sessions.len() > WORKER_SESSIONS {
            sessions.pop_front();
        }
    }

    /// Get the real token of the kept `surrogate` of `session`.
    fn real_of(&self, session: &str, surrogate: &str) -> Option<String> {
        self.0
            .borrow()
            .iter()
            .filter(|(id, _)| id == session)
            .flat_map(|(_, tokens)| tokens)
            .find(|t| t.surrogate == surrogate)
            .map(|t| t.real.clone())
    }

    /// Get all kept real tokens. An API answer must not contain them.
    fn reals(&self) -> Vec<String> {
        self.0
            .borrow()
            .iter()
            .flat_map(|(_, tokens)| tokens)
            .map(|t| t.real.clone())
            .collect()
    }
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

/// A result whose `Err` holds a local answer that refuses the request.
type Refusable<T> = Result<T, Response<ResponseBody>>;

/// Scan `resp` for the real values `known` and read it to its end.
/// Returns:
///   The parts and the body. The inner `Err` holds a local `502` if the
///   scan ended the body.
async fn scanned(
    resp: Response<ResponseBody>,
    known: Vec<String>,
) -> anyhow::Result<Refusable<(hyper::http::response::Parts, Bytes)>> {
    let (parts, body) = scan::scan_answer(resp, &FORMATS, known).await?.into_parts();
    // The scan ends the body with an error if it finds a real value.
    match oauth::read_body(body).await {
        Ok(bytes) => Ok(Ok((parts, bytes))),
        Err(_) => Ok(Err(oauth::server_error(
            "anthropic: a Remote Control bridge answer that the scan refused",
        ))),
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
/// Exception: Remote Control (`/remote-control`).
/// `POST /v1/code/sessions/{id}/bridge` with the user's credential gets a
/// worker token for the session (see [`Self::bridge`]). The interceptor
/// keeps the real token, and the guest gets a surrogate. On
/// `/v1/code/sessions/{id}/worker` and the paths below it, a request whose
/// only credential is a kept surrogate of the same session goes upstream
/// with the real token, with the same answer scan. The session id must
/// match exactly. Any other token, also a real worker token, gets
/// [`oauth::foreign_credential`]. Thus a guest cannot use a worker token
/// of a session in another account. The scan of all API answers also
/// searches for the kept real worker tokens.
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
///  * The worker tokens are kept only in memory. After an airlock restart,
///    the worker routes refuse the old surrogates until Claude Code calls
///    `bridge` again.
///  * The server mode of Remote Control (`claude remote-control`) does not
///    work. Its environment and session ingress tokens are not kept.
///  * Claude Code also connects to `mcp-proxy.anthropic.com` at run time.
///    It is not verified which credential it sends there. The service
///    does not own that host, so a surrogate sent there stays a surrogate.
pub struct Anthropic {
    endpoints: Endpoints,
    grants: Grants,
    codes: PendingCodes,
    targets: Vec<NetworkTarget>,
    workers: WorkerTokens,
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
            workers: WorkerTokens::default(),
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
            match session_route(req.uri().path(), &path) {
                Some((SessionRoute::Bridge, id)) if method == Method::POST => {
                    let id = id.to_string();
                    return self.bridge(to, req, &id, injected, next).await;
                }
                Some((SessionRoute::Worker, id)) => {
                    let real = only_bearer(req.headers()).and_then(|t| self.workers.real_of(id, t));
                    if let Some(real) = real {
                        tracing::debug!("anthropic: a worker token surrogate on {path} swapped");
                        let mut value = HeaderValue::from_str(&format!("Bearer {real}"))?;
                        value.set_sensitive(true);
                        req.headers_mut().insert(AUTHORIZATION, value);
                        return self
                            .grants
                            .forward_credentialed(req, injected, self.workers.reals(), next)
                            .await;
                    }
                }
                _ => {}
            }
            return self
                .grants
                .forward_api(to, req, injected, self.workers.reals(), next, sign_in_again)
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

    /// Handle `POST /v1/code/sessions/{id}/bridge` of Remote Control.
    ///
    /// The request gets the credential swap of [`Grants::swap_headers`],
    /// and the answer gets the scan for the real tokens of the store. The
    /// `worker_jwt` of a 2xx answer is kept for the session, and the guest
    /// gets a surrogate in its place ([`worker_surrogate`]). A final scan
    /// also searches the answer that the guest gets for all kept real
    /// worker tokens and the new one. The guest gets a local `502` and
    /// nothing is kept if:
    ///  * the guest sent no grant surrogate and no injected secret
    ///  * the `api_base_url` is not the API host
    ///  * the `worker_jwt` is not a JWT string
    ///  * the final scan finds a real value, for example a copy of the
    ///    worker token in another field.
    ///
    /// Args:
    ///  - `to`: The API endpoint
    ///  - `req`: The guest's request
    ///  - `session`: The session id of the path
    ///  - `injected`: Masked secrets that the inject rules put in
    ///  - `next`: The next step that sends the request upstream.
    ///
    /// Returns:
    ///   The scanned answer, or a local answer if the request or the
    ///   answer is refused.
    async fn bridge(
        &self,
        to: &Endpoint,
        mut req: Request<ResponseBody>,
        session: &str,
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
        let mut known = self.grants.known_reals(&swapped, injected).await?;
        let (parts, bytes) = oauth::forward_buffered(req, next).await?;
        // The upstream answer has the new real worker token, so this scan
        // searches only for the other real values.
        let (parts, bytes) = match scanned(oauth::rebuilt(parts, bytes), known.clone()).await? {
            Ok(read) => read,
            Err(refused) => return Ok(refused),
        };
        let (answer, token) = match self.rewrite_bridge(&swapped, parts, bytes)? {
            Ok(rewritten) => rewritten,
            Err(refused) => return Ok(refused),
        };
        known.extend(self.workers.reals());
        known.extend(token.iter().map(|t| t.real.clone()));
        let (parts, bytes) = match scanned(answer, known).await? {
            Ok(read) => read,
            Err(refused) => return Ok(refused),
        };
        // A local 502 of the scan is no success.
        if let Some(token) = token.filter(|_| parts.status.is_success()) {
            tracing::debug!("anthropic: a worker token for Remote Control session {session}");
            self.workers.add(session, token);
        }
        Ok(oauth::rebuilt(parts, bytes))
    }

    /// Put a surrogate in place of the `worker_jwt` of a 2xx `bridge`
    /// answer.
    /// Args:
    ///  - `swapped`: The credential swap of the request
    ///  - `parts`: The parts of the scanned answer
    ///  - `bytes`: The body of the scanned answer.
    ///
    /// Returns:
    ///   The answer for the guest and the worker token to keep, if the
    ///   answer has one. The inner `Err` holds a local `502`.
    fn rewrite_bridge(
        &self,
        swapped: &Swapped,
        parts: hyper::http::response::Parts,
        bytes: Bytes,
    ) -> anyhow::Result<Refusable<(Response<ResponseBody>, Option<WorkerToken>)>> {
        let unchanged = |parts, bytes| Ok(Ok((oauth::rebuilt(parts, bytes), None)));
        if !parts.status.is_success() {
            return unchanged(parts, bytes);
        }
        let Some(mut answer) = oauth::answer_object(&parts, &bytes) else {
            return unchanged(parts, bytes);
        };
        let real = match answer.get("worker_jwt") {
            None => return unchanged(parts, bytes),
            Some(Value::String(real)) => real.clone(),
            Some(_) => {
                return Ok(Err(oauth::server_error(
                    "anthropic: a Remote Control bridge answer with a worker_jwt that is no \
                     string",
                )));
            }
        };
        if matches!(swapped.credential, Credential::None) {
            return Ok(Err(oauth::server_error(
                "anthropic: a Remote Control bridge answer with a worker token for a request \
                 without a credential",
            )));
        }
        // Claude Code refuses a bridge answer without this field, so a
        // missing field is also refused.
        let base = answer.get("api_base_url").and_then(Value::as_str);
        if !base.is_some_and(|u| self.is_api_base(u)) {
            return Ok(Err(oauth::server_error(
                "anthropic: a Remote Control bridge answer with an api_base_url that is not the \
                 API host",
            )));
        }
        let Some(surrogate) = worker_surrogate(&real)? else {
            return Ok(Err(oauth::server_error(
                "anthropic: a Remote Control bridge answer with a worker_jwt that is no JWT",
            )));
        };
        answer.insert("worker_jwt".into(), surrogate.clone().into());
        let body = Bytes::from(Value::Object(answer).to_string());
        Ok(Ok((
            oauth::rebuilt(parts, body),
            Some(WorkerToken { surrogate, real }),
        )))
    }

    /// Whether `url` is the base URL of the API host, with or without a
    /// trailing `/`.
    fn is_api_base(&self, url: &str) -> bool {
        let url = url.strip_suffix('/').unwrap_or(url);
        // Host names are not case sensitive.
        url.strip_prefix("https://")
            .is_some_and(|host| host.eq_ignore_ascii_case(&self.endpoints.api.authority()))
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
        let mut known = self.grants.known_reals(&swapped, injected).await?;
        known.extend(self.workers.reals());
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
    //! Anthropic token formats: real tokens and minted surrogates. The
    //! limits of the worker token table.

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

    /// Test that the worker token table keeps the newest tokens and
    /// sessions. A long Remote Control use must not grow the table without
    /// a limit.
    ///   1. Add one token more than the limit to a session, and add one
    ///      real token two times
    ///   2. Check that the oldest token is gone and that the second add
    ///      kept one copy with the new surrogate
    ///   3. Add one session more than the limit
    ///   4. Check that the oldest session is gone
    #[test]
    fn worker_tokens_above_limits_remove_oldest_token_and_session() {
        let token = |surrogate: &str, real: &str| WorkerToken {
            surrogate: surrogate.into(),
            real: real.into(),
        };
        let workers = WorkerTokens::default();
        for i in 0..=WORKER_TOKENS_PER_SESSION {
            workers.add("s", token(&format!("s{i}"), &format!("r{i}")));
        }
        let last = WORKER_TOKENS_PER_SESSION;
        workers.add("s", token("new", &format!("r{last}")));
        assert_eq!(workers.real_of("s", "s0"), None);
        assert_eq!(workers.real_of("s", "s1").as_deref(), Some("r1"));
        assert_eq!(workers.real_of("s", &format!("s{last}")), None);
        assert_eq!(workers.real_of("s", "new"), Some(format!("r{last}")));
        assert_eq!(workers.reals().len(), WORKER_TOKENS_PER_SESSION);

        for i in 0..WORKER_SESSIONS {
            workers.add(&format!("s{i}"), token("t", "r"));
        }
        assert_eq!(workers.real_of("s", "s1"), None);
        assert_eq!(workers.real_of("s0", "t").as_deref(), Some("r"));
        assert_eq!(workers.reals().len(), WORKER_SESSIONS);
    }

    /// Test that a worker token surrogate keeps the prefix, the header and
    /// the claims of the real token, and that only a JWT gets one. Claude
    /// Code reads the claims of the token.
    ///   1. Make surrogates for JWTs with and without `sk-ant-si-`
    ///   2. Check that only the signature is different
    ///   3. Check that values that are no JWT get no surrogate
    #[test]
    fn worker_surrogate_changes_only_jwt_signature() {
        for real in ["sk-ant-si-eyJh.eyJz.c2ln", "eyJh.eyJz.c2ln"] {
            let surrogate = worker_surrogate(real).unwrap().unwrap();
            let (kept, signature) = surrogate.rsplit_once('.').unwrap();
            assert_eq!(kept, real.rsplit_once('.').unwrap().0);
            assert_ne!(signature, "c2ln");
            assert_eq!(
                URL_SAFE_NO_PAD.decode(signature).unwrap().len(),
                WORKER_SIGNATURE_BYTES
            );
        }
        for other in [
            "opaque",
            "eyJh.eyJz",
            "eyJh..c2ln",
            "a.b.c.d",
            "a.b.c d",
            "sk-ant-si-",
        ] {
            assert_eq!(worker_surrogate(other).unwrap(), None, "{other}");
        }
    }
}
