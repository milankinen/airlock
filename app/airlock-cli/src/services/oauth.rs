//! OAuth 2 logic shared by the services: the proxy's own HTTPS client for
//! token endpoints, strict parsing of token requests, the code exchange,
//! the swap of surrogates in API request headers, and the relay of the
//! agent's own refresh and revoke of grants.
//!
//! Provider specifics (endpoints, token formats, request shapes, error
//! codes) stay in the provider modules behind [`Provider`]; the surrogate
//! engine is [`super::tokens`].
//!
//! ## Fail closed
//!
//! - A token request (exchange, refresh, revoke) must have no query, a
//!   JSON or form body without duplicate keys, and a grant type of the
//!   provider's list; anything else gets a local error and is never
//!   forwarded. What goes upstream is re-serialized from the parsed
//!   fields, never the guest's bytes. An exchange must name its
//!   `client_id`.
//! - A 2xx token answer must be an uncompressed JSON object whose tokens
//!   are all in a format of the provider ([`super::tokens::collect`]), and
//!   carry the tokens the provider requires ([`Provider::exchange_requires`]),
//!   else the guest gets a local `502 server_error` and nothing is stored.
//! - The other answers of the token hosts' allowed routes pass
//!   [`backstop`]: a JSON answer up to [`BODY_LIMIT`] that carries a token
//!   field (`access_token`, `refresh_token`, `id_token`), a code field
//!   (`authorization_code`, `code`, `code_verifier`; a surrogate code
//!   airlock put there does not count) or a real token of the provider is
//!   refused. Larger or non-JSON answers pass unchanged: the providers
//!   issue their tokens in JSON token answers, which the services handle
//!   themselves. Other routes of the token hosts get [`route_not_allowed`].
//!   An allowed route goes upstream with its own path and no query
//!   ([`route_to`]), never the guest's spelling of it.
//! - API requests ask for an uncompressed answer (`Accept-Encoding:
//!   identity`, whatever the guest sent), and every API answer streams
//!   through [`super::scan::scan_answer`]: a compressed answer, a real
//!   token in a response header, or a real value the proxy knows (the
//!   store's tokens, the request's injected secrets) or a real token shape
//!   anywhere in the body or trailers is refused (or the stream ends
//!   before the chunk with it). A 401 from
//!   the API passes through unchanged: refreshing is the agent's job, as
//!   on a host.
//! - API credentials ([`Grants::swap_headers`]): only `Authorization:
//!   Bearer <token>` and `x-api-key: <value>` change, and only when the
//!   whole token or value is a known access or API-key surrogate of the
//!   service. Every other header keeps its surrogates: an upstream may
//!   reflect a header (an `Origin` into
//!   `Access-Control-Allow-Origin`, a value into an error message).
//!   Request bodies are never changed. The credentials are strict: an
//!   `Authorization` or `x-api-key` value must be such a surrogate, or a
//!   masked secret airlock injected; an unknown surrogate gets the
//!   provider's "sign in again", anything else (a refresh or ID-token
//!   surrogate too) [`foreign_credential`]. Neither goes upstream.
//! - The authority of every request is the endpoint's
//!   ([`pin_authority`]).
//!
//! ## Refresh relay
//!
//! The agent refreshes its own tokens; the proxy only relays that call
//! ([`Grants::relay_refresh`]): look up the grant by its refresh
//! surrogate in the store as it is now (another process may already have
//! rotated the real refresh token), send a refresh built by the provider
//! ([`Provider::refresh_body`]: the provider's own fields, the grant's
//! stored client id and the real refresh token, nothing of the guest's
//! request) upstream with the proxy's own [`Upstream`] (never the guest's
//! connection) in a task of its own — a dropped guest request does not
//! lose the rotated tokens — and store the answer
//! ([`super::tokens::apply_refresh`]) in one transaction on the grant's
//! current record. A sign-out that finishes while the refresh is in
//! flight leaves no record to store on: the new tokens are revoked
//! instead. The guest gets surrogates with the upstream's own
//! `expires_in` / real `exp` — never a minimum or a synthetic one. Races
//! between two refreshes of the same grant (two sandboxes, a retry) are
//! the agent's problem, as they would be on a host; the store only
//! guarantees that each write is atomic.
//!
//! ## Revoke
//!
//! A sign-out with an access or refresh surrogate deletes the grant and
//! revokes its real refresh token, then its access token where the
//! provider revokes access tokens ([`Provider::revokes_access_tokens`]),
//! with the grant's stored client id. A new sign-in that replaces older
//! grants revokes them too, in the background.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Either, Full, Limited};
use hyper::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
    HeaderMap, HeaderName, HeaderValue, TRANSFER_ENCODING, USER_AGENT,
};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, json};

use super::auth_codes::{Channel, PREFIX, PendingCodes};
use super::store::{Grant, NewGrant, Snapshot, TokenStore, random_bytes};
use super::tokens::{self, Formats, TokenKind};
use super::{ServiceId, scan};
use crate::network::http::ResponseBody;
use crate::network::interceptor::Next;
use crate::network::target::{Endpoint, InjectedSecret};

/// Cap on token-endpoint bodies the proxy reads (both directions).
pub const BODY_LIMIT: usize = 64 * 1024;

/// Bound on one call of the proxy to a token endpoint.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);

/// Keys of a JSON answer that carry tokens.
const TOKEN_KEYS: &[&str] = &["access_token", "refresh_token", "id_token"];

/// Keys of a JSON answer that carry an authorization code or its PKCE
/// verifier. A code value that is a surrogate code
/// ([`super::auth_codes::PREFIX`]) does not count: airlock put it there.
const CODE_KEYS: &[&str] = &["authorization_code", "code", "code_verifier"];

/// The header of a credential that is no `Authorization`.
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

/// The kinds of token an OAuth token answer may carry.
const OAUTH_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::Refresh, TokenKind::Id];

/// The surrogate kinds an API credential may be.
const CREDENTIAL_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::ApiKey];

/// The surrogate kinds that sign a grant out.
const SIGN_OUT_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::Refresh];

/// Who a new grant belongs to, from the exchange answer.
pub struct Account {
    /// The provider's id of the account; `None`: the answer names none
    /// (the grant then replaces no other).
    pub id: Option<String>,
    /// The email address, for `airlock show`.
    pub email: Option<String>,
    pub organization: Option<String>,
}

/// What a provider adds to the shared exchange, refresh relay and revoke.
/// `Sync`: [`Grants::relay_refresh`] moves the `&'static dyn Provider`
/// into a task of its own.
pub trait Provider: Sync {
    fn id(&self) -> ServiceId;

    /// The provider's token formats.
    fn formats(&self) -> &'static Formats;

    /// Path of the token endpoint (exchange, refresh).
    fn token_path(&self) -> &'static str;

    /// Path of the revoke endpoint.
    fn revoke_path(&self) -> &'static str;

    /// The main tokens an exchange answer must carry.
    fn exchange_requires(&self) -> &'static [TokenKind];

    /// The account of an exchange `answer` (the real tokens still in it).
    fn account(&self, answer: &Map<String, Value>) -> Account;

    /// The JSON body of a refresh of `grant` with its real
    /// `refresh_token`: from the provider's own fields and the grant's
    /// stored client id and scopes (an allowlist), never the guest's.
    fn refresh_body(&self, grant: &Grant, refresh_token: &str) -> Value;

    /// Whether the revoke endpoint takes access tokens too (else a
    /// sign-out revokes the refresh token only).
    fn revokes_access_tokens(&self) -> bool;

    /// Whether the revoke of a token of `hint` (`access_token`,
    /// `refresh_token`) names the client, as the agent's own revoke does.
    fn revoke_names_client(&self, _hint: &str) -> bool {
        true
    }
}

/// HTTPS client of the proxy itself, for the token endpoint of a
/// provider. Uses the proxy's TLS client config (system roots).
#[derive(Clone)]
pub struct Upstream {
    tls: Arc<rustls::ClientConfig>,
    endpoint: Endpoint,
}

impl Upstream {
    pub fn new(tls: Arc<rustls::ClientConfig>, endpoint: Endpoint) -> Self {
        Self { tls, endpoint }
    }

    /// `POST path` with a JSON `body`. Returns the status and the body
    /// (capped).
    pub async fn post_json(&self, path: &str, body: &Value) -> anyhow::Result<(StatusCode, Bytes)> {
        tokio::time::timeout(
            UPSTREAM_TIMEOUT,
            self.post_unbounded(path, body.to_string().into_bytes()),
        )
        .await
        .map_err(|_| anyhow!("{} did not answer in time", self.endpoint.host()))?
    }

    async fn post_unbounded(
        &self,
        path: &str,
        body: Vec<u8>,
    ) -> anyhow::Result<(StatusCode, Bytes)> {
        let host = self.endpoint.host();
        let port = self.endpoint.port();
        let tcp = tokio::net::TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connect to {host}:{port}"))?;
        let mut config = (*self.tls).clone();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| anyhow!("invalid hostname {host}: {e}"))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .with_context(|| format!("TLS to {host}"))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!("token endpoint connection: {e}");
            }
        });
        let req = Request::post(path)
            .header(HOST, self.endpoint.authority())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .header(ACCEPT_ENCODING, "identity")
            .header(USER_AGENT, concat!("airlock/", env!("CARGO_PKG_VERSION")))
            .body(Full::new(Bytes::from(body)))?;
        let resp = sender.send_request(req).await?;
        let status = resp.status();
        let body = read_body(resp.into_body()).await?;
        Ok((status, body))
    }
}

/// Read a whole body, at most [`BODY_LIMIT`] bytes.
pub async fn read_body<B>(body: B) -> anyhow::Result<Bytes>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    Limited::new(body, BODY_LIMIT)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|e| anyhow!("read a token endpoint body: {e}"))
}

/// A response with a JSON body, built by the proxy.
pub fn json_response(status: StatusCode, value: &Value) -> Response<ResponseBody> {
    let mut resp = Response::new(Either::Right(Full::new(Bytes::from(value.to_string()))));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// A token-endpoint error answered by the proxy: `{"error": code}`. The
/// log gets a warning with `why`: what airlock refused and why (never a
/// secret value), so a failing sign-in can be traced.
pub fn token_error(
    status: StatusCode,
    code: &str,
    why: impl std::fmt::Display,
) -> Response<ResponseBody> {
    tracing::warn!("refused: {why} (answered {} {code})", status.as_u16());
    json_response(status, &json!({ "error": code }))
}

/// The answer to an answer (or request) the proxy refuses to pass on:
/// `502`, logged with `why` (see [`token_error`]).
pub fn server_error(why: impl std::fmt::Display) -> Response<ResponseBody> {
    token_error(StatusCode::BAD_GATEWAY, "server_error", why)
}

/// Rebuild a response from its parts and a new (or re-read) body.
pub fn rebuilt(mut parts: hyper::http::response::Parts, body: Bytes) -> Response<ResponseBody> {
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(TRANSFER_ENCODING);
    Response::from_parts(parts, Either::Right(Full::new(body)))
}

/// Forward `req` asking for an uncompressed answer, and read the whole
/// response: for answers the proxy must parse.
pub async fn forward_buffered(
    mut req: Request<ResponseBody>,
    next: Next,
) -> anyhow::Result<(hyper::http::response::Parts, Bytes)> {
    req.headers_mut()
        .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let (parts, body) = next(req).await?.into_parts();
    let body = read_body(body).await?;
    Ok((parts, body))
}

/// The JSON object of a 2xx answer the proxy must rewrite. `None` when it
/// is compressed or no JSON object: the guest then gets
/// [`server_error`].
pub fn answer_object(
    parts: &hyper::http::response::Parts,
    body: &[u8],
) -> Option<Map<String, Value>> {
    if is_encoded(&parts.headers) {
        return None;
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The body has a `Content-Encoding` other than `identity`.
fn is_encoded(headers: &HeaderMap) -> bool {
    headers
        .get_all(CONTENT_ENCODING)
        .iter()
        .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
}

/// Whether a JSON value carries a token or a code: a token field, a code
/// field (unless it holds a surrogate code, see [`CODE_KEYS`]), or a real
/// token of `formats`.
pub fn carries_token(value: &Value, formats: &Formats) -> bool {
    token_field(value, formats, &[], "$").is_some()
}

/// Where a JSON value carries a token or a code (see [`carries_token`]),
/// as a path such as `$.account.id`, for the log: never the value. The
/// string values of the keys in `passed` are not checked for real token
/// formats (identifiers of a flow that look like tokens, for example the
/// device sign-in's `device_auth_id`); their key names still count.
fn token_field(value: &Value, formats: &Formats, passed: &[&str], path: &str) -> Option<String> {
    find_token(value, formats, passed, path, false)
}

/// [`token_field`] below `path`. `in_error`: the value is inside an
/// `error` or `errors` field, where `code` is an error code (for example
/// the device poll's `deviceauth_authorization_pending`), not an
/// authorization code; its value is still checked for token formats.
fn find_token(
    value: &Value,
    formats: &Formats,
    passed: &[&str],
    path: &str,
    in_error: bool,
) -> Option<String> {
    match value {
        Value::String(s) => formats.is_real(s).then(|| path.to_string()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_token(v, formats, passed, &format!("{path}[{i}]"), in_error)),
        Value::Object(map) => map.iter().find_map(|(k, v)| {
            let key = k.as_str();
            let at = format!("{path}.{key}");
            let surrogate_code = matches!(v, Value::String(c) if c.starts_with(PREFIX));
            let error_code = in_error && key == "code";
            if TOKEN_KEYS.contains(&key)
                || (CODE_KEYS.contains(&key) && !surrogate_code && !error_code)
            {
                return Some(at);
            }
            if passed.contains(&key) && v.is_string() {
                return None;
            }
            let in_error = in_error || key == "error" || key == "errors";
            find_token(v, formats, passed, &at, in_error)
        }),
        _ => None,
    }
}

/// Pin the authority of `req` to the endpoint the guest connected to,
/// before the service swaps or forwards anything: whatever `Host`,
/// absolute-form target or `:authority` the guest sent, the upstream sees
/// `to`. HTTP/2 requests get an `https` URI with the endpoint's authority
/// (and no `Host`); others get the origin form and `Host`.
pub fn pin_authority(req: &mut Request<ResponseBody>, to: &Endpoint) -> anyhow::Result<()> {
    let authority = to.authority();
    let path = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    let uri = if req.version() == hyper::Version::HTTP_2 {
        req.headers_mut().remove(HOST);
        format!("https://{authority}{path}")
    } else {
        req.headers_mut().insert(
            HOST,
            HeaderValue::from_str(&authority).context("endpoint authority")?,
        );
        path
    };
    *req.uri_mut() = uri.parse().context("pinned request URI")?;
    Ok(())
}

/// Forward a request of an allowed route of a token host, asking for an
/// uncompressed answer, and pass the answer through [`backstop`]. Also
/// for a host the service does not know. Never with a token swap.
pub async fn forward_auth_host(
    mut req: Request<ResponseBody>,
    next: Next,
    formats: &Formats,
    passed: &[&str],
) -> anyhow::Result<Response<ResponseBody>> {
    req.headers_mut()
        .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    backstop(next(req).await?, formats, passed).await
}

/// The local answer to a route of a token host that no service rule
/// allows: `403`, never forwarded. The log names the method and path.
pub fn route_not_allowed(
    service: ServiceId,
    to: &Endpoint,
    req: &Request<ResponseBody>,
) -> Response<ResponseBody> {
    tracing::warn!(
        "refused: {}: {} {} on {} is no route of the sign-in (answered 403)",
        service.name(),
        req.method(),
        req.uri().path(),
        to.host()
    );
    json_response(
        StatusCode::FORBIDDEN,
        &json!({
            "error": "airlock_route_not_allowed",
            "error_description": format!(
                "airlock: with [network.services] {} enabled, {} serves only the routes of \
                 the agent's sign-in",
                service.name(),
                to.host()
            ),
        }),
    )
}

/// The last check of an answer of a token host (see the module docs): a
/// JSON answer up to [`BODY_LIMIT`] is read and refused when it
/// [`carries_token`]; JSON that is compressed (the proxy asked for none)
/// is refused unread. Larger or non-JSON answers pass unchanged. The
/// string values of the keys in `passed` are not checked for token
/// formats (see [`token_field`]).
pub async fn backstop(
    resp: Response<ResponseBody>,
    formats: &Formats,
    passed: &[&str],
) -> anyhow::Result<Response<ResponseBody>> {
    if !is_json(resp.headers()) || content_length(resp.headers()).is_some_and(|n| n > BODY_LIMIT) {
        return Ok(resp);
    }
    if is_encoded(resp.headers()) {
        return Ok(server_error("a compressed JSON answer of an auth host"));
    }
    let (parts, body) = resp.into_parts();
    let body = read_body(body).await?;
    if let Ok(value) = serde_json::from_slice::<Value>(&body)
        && let Some(field) = token_field(&value, formats, passed, "$")
    {
        return Ok(server_error(format_args!(
            "an auth host answer that carries a token at {field}"
        )));
    }
    Ok(rebuilt(parts, body))
}

/// The `Content-Type` is JSON (`application/json` or `+json`).
fn is_json(headers: &HeaderMap) -> bool {
    mime_type(headers).is_some_and(|mime| mime == "application/json" || mime.ends_with("+json"))
}

/// The media type of the `Content-Type`, lowercase, without parameters.
fn mime_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
}

/// The `Content-Length`, if it is one number.
fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
}

/// Whether `value` (a credential header's value, or the token of a
/// `Bearer` one) is the real value of a masked secret that the inject
/// rules put into this request: airlock injected it, so it is the user's.
pub fn is_injected(value: &str, injected: &[InjectedSecret]) -> bool {
    !value.is_empty() && injected.iter().any(|s| s.real == value)
}

/// The local answer to an API request with a credential that is neither
/// a surrogate of the service nor a masked secret airlock injected.
pub fn foreign_credential(service: ServiceId, to: &Endpoint) -> Response<ResponseBody> {
    let name = service.name();
    tracing::warn!(
        "refused: {name}: an API request to {} with a credential that is no access or API-key \
         surrogate of the service and no injected secret (answered 401)",
        to.host()
    );
    json_response(
        StatusCode::UNAUTHORIZED,
        &json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "code": "airlock_foreign_credential",
                "message": format!(
                    "airlock: with [network.services] {name} enabled, credentials for {} \
                     must come from the {name} sign-in in the sandbox or from a masked \
                     [env] secret with inject",
                    to.host()
                ),
            },
        }),
    )
}

/// A random account id for a sign-in whose answer names no account: such
/// a grant never replaces another.
pub fn random_account_id() -> anyhow::Result<String> {
    Ok(format!(
        "airlock-random-{}",
        hex::encode(random_bytes::<16>()?)
    ))
}

/// The token of an `Authorization: Bearer <token>` value.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// The URI of `uri` with the path `path` and no query: what an allowed
/// route forwards, never the guest's spelling of it. Keeps the scheme
/// and authority of an HTTP/2 request ([`pin_authority`]).
fn routed_uri(uri: &hyper::Uri, path: &str) -> anyhow::Result<hyper::Uri> {
    let uri = match (uri.scheme_str(), uri.authority()) {
        (Some(scheme), Some(authority)) => format!("{scheme}://{authority}{path}"),
        _ => path.to_string(),
    };
    uri.parse().context("route URI")
}

/// Send `req` to the route `path`: its URI gets that path and no query
/// (see [`routed_uri`]).
pub fn route_to(req: &mut Request<ResponseBody>, path: &str) -> anyhow::Result<()> {
    *req.uri_mut() = routed_uri(req.uri(), path)?;
    Ok(())
}

/// The `scope` of a token answer, split.
pub fn scopes_of(answer: &Map<String, Value>) -> Vec<String> {
    answer
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// A request path made canonical for matching: percent-decoded, `;`
/// parameters, empty and `.` segments removed, `..` applied, no trailing
/// `/`, ASCII lowercase.
pub fn normalize_path(path: &str) -> String {
    let decoded = percent_decode(path);
    let mut segments: Vec<&str> = Vec::new();
    for segment in decoded.split('/') {
        let segment = segment.split(';').next().unwrap_or_default();
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    format!("/{}", segments.join("/")).to_ascii_lowercase()
}

/// `%XX` escapes decoded; invalid escapes stay as they are.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(hex), 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The format of a token request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyFormat {
    Json,
    Form,
}

/// A token request, parsed strictly (see the module docs).
pub struct TokenRequest {
    pub format: BodyFormat,
    pub fields: Map<String, Value>,
}

impl TokenRequest {
    /// Read and parse `req`. `Err` holds the local answer to a request
    /// that is refused.
    pub async fn read(
        req: Request<ResponseBody>,
    ) -> anyhow::Result<Result<(hyper::http::request::Parts, Self), Response<ResponseBody>>> {
        let path = req.uri().path().to_string();
        let invalid = |why: &str| {
            Ok(Err(token_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format_args!("a token request to {path}: {why}"),
            )))
        };
        if req.uri().query().is_some() {
            return invalid("it has a query");
        }
        if is_encoded(req.headers()) {
            return invalid("its body is compressed");
        }
        let format = match req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| ct.split(';').next())
            .map(|mime| mime.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("application/json") => BodyFormat::Json,
            Some("application/x-www-form-urlencoded") => BodyFormat::Form,
            _ => return invalid("its content type is not JSON or a form"),
        };
        let (parts, body) = req.into_parts();
        let bytes = read_body(body).await?;
        let fields = match format {
            BodyFormat::Json => serde_json::from_slice::<UniqueObject>(&bytes)
                .ok()
                .map(|o| o.0),
            BodyFormat::Form => unique_form(&bytes),
        };
        let Some(fields) = fields else {
            return invalid("its body does not parse, or a field appears twice");
        };
        Ok(Ok((parts, Self { format, fields })))
    }

    /// The string field `name`.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }

    /// The request to forward: `parts` with `path`, and a body serialized
    /// from the fields in the format the guest used.
    pub fn into_request(
        self,
        mut parts: hyper::http::request::Parts,
        path: &str,
    ) -> anyhow::Result<Request<ResponseBody>> {
        let (content_type, body) = match self.format {
            BodyFormat::Json => ("application/json", Value::Object(self.fields).to_string()),
            BodyFormat::Form => {
                let mut form = url::form_urlencoded::Serializer::new(String::new());
                for (k, v) in &self.fields {
                    form.append_pair(k, v.as_str().unwrap_or_default());
                }
                ("application/x-www-form-urlencoded", form.finish())
            }
        };
        parts.uri = routed_uri(&parts.uri, path)?;
        parts.headers.remove(CONTENT_LENGTH);
        parts.headers.remove(TRANSFER_ENCODING);
        parts
            .headers
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        Ok(Request::from_parts(
            parts,
            Either::Right(Full::new(Bytes::from(body))),
        ))
    }
}

/// A JSON object whose keys are unique.
struct UniqueObject(Map<String, Value>);

impl<'de> serde::Deserialize<'de> for UniqueObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueObject;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object with unique keys")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<UniqueObject, A::Error> {
                let mut map = Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    if map.contains_key(&key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    let value: Value = access.next_value()?;
                    map.insert(key, value);
                }
                Ok(UniqueObject(map))
            }
        }
        deserializer.deserialize_map(UniqueVisitor)
    }
}

/// The fields of a form, `None` when a key repeats.
fn unique_form(body: &[u8]) -> Option<Map<String, Value>> {
    let mut seen = HashSet::new();
    let mut map = Map::new();
    for (k, v) in url::form_urlencoded::parse(body) {
        if !seen.insert(k.clone()) {
            return None;
        }
        map.insert(k.into_owned(), Value::String(v.into_owned()));
    }
    Some(map)
}

/// Swap the surrogate code of an exchange for the real one. `false` when
/// the exchange is refused: its code is no surrogate this process issued
/// to `service` through the channel its `redirect_uri` names
/// ([`Channel::of_redirect`]; `device_redirect` is the provider's
/// device-flow redirect). `manual_redirect` names the one `redirect_uri`
/// whose code the user brings by hand: it is real already and passes
/// unchanged, but only when the exchange's `code_verifier` belongs to a
/// sign-in page the browser bridge opened for `service`
/// ([`PendingCodes::redeem_page`]), so the sandbox cannot bring a code of
/// a sign-in the host never opened (another account's).
pub fn swap_code(
    token: &mut TokenRequest,
    codes: &PendingCodes,
    service: ServiceId,
    manual_redirect: Option<&str>,
    device_redirect: Option<&str>,
) -> Result<(), &'static str> {
    let (Some(code), Some(redirect)) = (token.field("code"), token.field("redirect_uri")) else {
        return Err("the exchange has no code or no redirect_uri");
    };
    if manual_redirect == Some(redirect) {
        if code.starts_with(PREFIX) {
            return Err("the manual exchange carries a surrogate code");
        }
        let Some(verifier) = token.field("code_verifier") else {
            return Err("the manual exchange has no code_verifier");
        };
        if !codes.redeem_page(verifier, service) {
            return Err(
                "the manual exchange's code_verifier matches no sign-in page that the browser \
                 bridge opened in the last 10 minutes (or that page was used)",
            );
        }
        return Ok(());
    }
    let Some(channel) = Channel::of_redirect(redirect, device_redirect) else {
        return Err("the exchange's redirect_uri is no callback port or device flow of airlock");
    };
    let Some(real) = codes.redeem(code, service, channel) else {
        return Err(
            "the exchange's code is no surrogate code that airlock issued for this service and \
             channel in the last 10 minutes (or it was used)",
        );
    };
    token.fields.insert("code".into(), Value::String(real));
    Ok(())
}

/// The error code of a token-endpoint error body: `{"error": "code"}` or
/// `{"error": {"code": "..."}}`.
pub fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    match value.get("error")? {
        Value::String(code) => Some(code.clone()),
        Value::Object(err) => err
            .get("code")
            .or_else(|| err.get("type"))
            .and_then(Value::as_str)
            .map(String::from),
        _ => None,
    }
}

/// An upstream token-endpoint error relayed to the guest unchanged, once
/// checked for a leaked real token (the backstop of the module docs
/// applies here too, even on an error answer).
fn relay_error(status: StatusCode, bytes: &[u8], formats: &Formats) -> Response<ResponseBody> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) if !carries_token(&value, formats) => {
            tracing::info!(
                "token endpoint answered {}{}",
                status.as_u16(),
                error_code(bytes)
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default()
            );
            json_response(status, &value)
        }
        _ => server_error(format_args!(
            "a token-endpoint error answer ({}) that carries a token or is no JSON",
            status.as_u16()
        )),
    }
}

/// Who an API request's credential belongs to.
pub enum Credential {
    /// The request names no credential.
    None,
    /// A surrogate of the grant with this id: its real value went in.
    Grant(String),
    /// A masked secret the inject rules put in: passes unchanged.
    Injected,
}

/// What [`Grants::swap_headers`] found.
pub struct Swapped {
    pub credential: Credential,
    /// The store, when the swap read it.
    pub snapshot: Option<Arc<Snapshot>>,
}

/// The grant lifecycle of one service in this process: the code exchange,
/// the credential swap on API requests, the refresh relay, and revoke.
pub struct Grants {
    provider: &'static dyn Provider,
    pub store: Arc<TokenStore>,
    upstream: Upstream,
    /// The warning that the store cannot be read was logged.
    store_warned: std::cell::Cell<bool>,
}

impl Grants {
    pub fn new(
        provider: &'static dyn Provider,
        store: Arc<TokenStore>,
        upstream: Upstream,
    ) -> Self {
        Self {
            provider,
            store,
            upstream,
            store_warned: std::cell::Cell::new(false),
        }
    }

    fn service(&self) -> ServiceId {
        self.provider.id()
    }

    fn formats(&self) -> &'static Formats {
        self.provider.formats()
    }

    /// The credential swap of an API request (see the module docs): an
    /// `Authorization: Bearer <surrogate>` or `x-api-key: <surrogate>`
    /// whose whole token is a known access or API-key surrogate of the
    /// service gets the real value; an injected masked secret passes. No
    /// other header changes. The store is read only when a credential is
    /// surrogate-shaped. `Err` holds the local answer: `sign_in_again` for
    /// a surrogate airlock does not know (signed out, or never issued),
    /// [`foreign_credential`] for any other value (a refresh or ID-token
    /// surrogate included: those are no API credential).
    pub async fn swap_headers(
        &self,
        to: &Endpoint,
        headers: &mut HeaderMap,
        injected: &[InjectedSecret],
        sign_in_again: fn() -> Response<ResponseBody>,
    ) -> anyhow::Result<Result<Swapped, Response<ResponseBody>>> {
        let formats = self.formats();
        let mut credential = Credential::None;
        let mut snapshot: Option<Arc<Snapshot>> = None;
        for name in [AUTHORIZATION, X_API_KEY] {
            let values: Vec<HeaderValue> = headers.get_all(&name).iter().cloned().collect();
            if values.is_empty() {
                continue;
            }
            headers.remove(&name);
            for value in values {
                let text = value.to_str().unwrap_or_default();
                let token = if name == AUTHORIZATION {
                    bearer_token(text)
                } else {
                    Some(text)
                };
                let Some(surrogate) = token.filter(|t| formats.is_surrogate(t)) else {
                    if is_injected(text, injected)
                        || token.is_some_and(|t| is_injected(t, injected))
                    {
                        if matches!(credential, Credential::None) {
                            credential = Credential::Injected;
                        }
                        headers.append(&name, value);
                        continue;
                    }
                    return Ok(Err(foreign_credential(self.service(), to)));
                };
                if snapshot.is_none() {
                    snapshot = Some(self.store.snapshot(self.service()).await?);
                }
                let Some(resolved) = snapshot.as_ref().and_then(|s| s.resolve(surrogate)) else {
                    return Ok(Err(sign_in_again()));
                };
                if !CREDENTIAL_KINDS.contains(&resolved.kind) {
                    return Ok(Err(foreign_credential(self.service(), to)));
                }
                let real = if name == AUTHORIZATION {
                    format!("Bearer {}", resolved.real)
                } else {
                    resolved.real
                };
                let mut real = HeaderValue::from_str(&real)
                    .map_err(|_| anyhow!("a stored token is not a valid header value"))?;
                real.set_sensitive(true);
                headers.append(&name, real);
                credential = Credential::Grant(resolved.grant_id);
            }
        }
        Ok(Ok(Swapped {
            credential,
            snapshot,
        }))
    }

    /// An API request: [`Self::swap_headers`], then forwarded asking for
    /// an uncompressed answer, whatever the guest accepts, so that the
    /// answer (a 401 included: refreshing is the agent's job) passes
    /// [`scan::scan_answer`]. The body goes as the guest sent it.
    pub async fn forward_api(
        &self,
        to: &Endpoint,
        mut req: Request<ResponseBody>,
        injected: &[InjectedSecret],
        next: Next,
        sign_in_again: fn() -> Response<ResponseBody>,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let swapped = match self
            .swap_headers(to, req.headers_mut(), injected, sign_in_again)
            .await?
        {
            Ok(swapped) => swapped,
            Err(refused) => return Ok(refused),
        };
        let known = self.known_reals(&swapped, injected).await?;
        req.headers_mut()
            .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        scan::scan_answer(next(req).await?, self.formats(), known).await
    }

    /// The real values an API answer must not carry
    /// ([`scan::known_reals`]): of the store (read now when the swap did
    /// not) and of the secrets injected into the request.
    pub async fn known_reals(
        &self,
        swapped: &Swapped,
        injected: &[InjectedSecret],
    ) -> anyhow::Result<Vec<String>> {
        let snapshot = match &swapped.snapshot {
            Some(snapshot) => snapshot.clone(),
            None => match self.store.snapshot(self.service()).await {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    // Fail closed: no scan, no answer.
                    if !self.store_warned.replace(true) {
                        tracing::warn!(
                            "{}: cannot read the token store to scan API answers, so they \
                             fail: {e:#}",
                            self.service().name()
                        );
                    }
                    return Err(e.context("read the token store for the answer scan"));
                }
            },
        };
        Ok(scan::known_reals(&snapshot, injected))
    }

    /// Forward a code exchange (its code already swapped, see
    /// [`swap_code`]); keep the real tokens as a new grant with the
    /// exchange's `client_id`, and answer with surrogates.
    pub async fn exchange(
        &self,
        parts: hyper::http::request::Parts,
        token: TokenRequest,
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let Some(client_id) = token
            .field("client_id")
            .filter(|c| !c.is_empty())
            .map(String::from)
        else {
            return Ok(token_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format_args!(
                    "{}: a code exchange without client_id",
                    self.service().name()
                ),
            ));
        };
        let req = token.into_request(parts, self.provider.token_path())?;
        let formats = self.formats();
        let (parts, bytes) = forward_buffered(req, next).await?;
        if !parts.status.is_success() {
            return backstop(rebuilt(parts, bytes), formats, &[]).await;
        }
        let Some(answer) = answer_object(&parts, &bytes) else {
            return Ok(server_error(format_args!(
                "{}: a sign-in answer that is no uncompressed JSON object",
                self.service().name()
            )));
        };
        let found = match tokens::collect(formats, &answer, OAUTH_KINDS, true) {
            Ok(found) => found,
            Err(refusal) => {
                return Ok(server_error(format_args!(
                    "{}: a sign-in answer: {refusal}",
                    self.service().name()
                )));
            }
        };
        let required = self.provider.exchange_requires();
        if let Some(missing) = required
            .iter()
            .find(|kind| !found.iter().any(|f| f.kind == **kind && f.primary))
        {
            return Ok(server_error(format_args!(
                "{}: a sign-in answer without a {missing:?} token",
                self.service().name()
            )));
        }
        let account = self.provider.account(&answer);
        let tokens = tokens::mint_all(&found)?;
        let surrogates = tokens
            .iter()
            .map(|t| (t.real.clone(), t.surrogate.clone()))
            .collect();
        self.insert_grant(NewGrant {
            account_id: match account.id {
                Some(id) => id,
                None => random_account_id()?,
            },
            account: account.email,
            organization: account.organization,
            client_id,
            scopes: scopes_of(&answer),
            tokens,
        })
        .await?;
        tracing::debug!("{}: stored a new sign-in", self.service().name());
        let mut answer = Value::Object(answer);
        tokens::substitute(&mut answer, &surrogates);
        Ok(rebuilt(parts, Bytes::from(answer.to_string())))
    }

    /// Store a new grant (see [`TokenStore::insert_grant`]); the grants it
    /// replaces are revoked upstream in the background, best effort.
    pub async fn insert_grant(&self, new: NewGrant) -> anyhow::Result<Grant> {
        let (grant, replaced) = self.store.insert_grant(self.service(), new).await?;
        for old in replaced {
            let (provider, upstream) = (self.provider, self.upstream.clone());
            tokio::task::spawn_local(async move {
                revoke_tokens(
                    provider,
                    &upstream,
                    &old.client_id,
                    old.real(TokenKind::Access),
                    old.real(TokenKind::Refresh),
                )
                .await;
            });
        }
        Ok(grant)
    }

    /// A sign-out: when `token` is an access or refresh surrogate of a
    /// grant, delete the grant and revoke its real tokens upstream (see
    /// [`revoke_tokens`]) with the grant's client id. Runs in a task of
    /// its own, so a dropped guest request still completes it. Unknown
    /// tokens are ignored; the guest always gets `200 {}`.
    pub async fn revoke(&self, token: Option<&str>) -> anyhow::Result<Response<ResponseBody>> {
        let done = || json_response(StatusCode::OK, &json!({}));
        let Some(token) = token.filter(|t| self.formats().is_surrogate(t)) else {
            return Ok(done());
        };
        let service = self.service();
        let Some(grant) = self.store.grant_of(service, token, SIGN_OUT_KINDS).await? else {
            return Ok(done());
        };
        let provider = self.provider;
        let store = self.store.clone();
        let upstream = self.upstream.clone();
        let task = tokio::task::spawn_local(async move {
            if let Err(e) = store.delete_grant(service, &grant.id).await {
                tracing::warn!("{}: delete a sign-in: {e:#}", service.name());
            }
            revoke_tokens(
                provider,
                &upstream,
                &grant.client_id,
                grant.real(TokenKind::Access),
                grant.real(TokenKind::Refresh),
            )
            .await;
            tracing::debug!("{}: signed out", service.name());
        });
        task.await.context("revoke task")?;
        Ok(done())
    }

    /// Relay the agent's own refresh of the grant of the refresh surrogate
    /// in `token.fields["refresh_token"]` (see the module docs); the rest
    /// of the guest's request stays here. A request with no
    /// `refresh_token` field at all is malformed (`400 invalid_request`);
    /// `unknown()` answers one that has the field but not as a surrogate
    /// airlock knows (signed out, never issued, or a grant with no refresh
    /// token).
    pub async fn relay_refresh(
        &self,
        token: TokenRequest,
        unknown: fn() -> Response<ResponseBody>,
    ) -> anyhow::Result<Response<ResponseBody>> {
        if token.field("refresh_token").is_none() {
            return Ok(token_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format_args!("{}: a refresh without refresh_token", self.service().name()),
            ));
        }
        let Some(surrogate) = token
            .field("refresh_token")
            .filter(|t| self.formats().is_surrogate(t))
            .map(String::from)
        else {
            return Ok(unknown());
        };
        let provider = self.provider;
        let store = self.store.clone();
        let upstream = self.upstream.clone();
        // A task of its own: a guest that drops the request mid-flight
        // must not lose the rotated tokens (see the module docs).
        let task = tokio::task::spawn_local(async move {
            relay_one(provider, &store, &upstream, &surrogate).await
        });
        match task.await.context("refresh relay task")?? {
            Relayed::Unknown => Ok(unknown()),
            Relayed::Refused(resp) => Ok(resp),
            Relayed::Answer(answer) => Ok(json_response(StatusCode::OK, &answer)),
        }
    }
}

/// What relaying one refresh produced.
enum Relayed {
    /// No grant for the surrogate (deleted, or never issued).
    Unknown,
    /// The upstream refused the refresh, or its answer is refused; the
    /// guest's answer.
    Refused(Response<ResponseBody>),
    /// Stored; the guest's answer (surrogates in place of the real
    /// tokens).
    Answer(Value),
}

/// Look up the grant of `surrogate`, send the provider's refresh of it
/// ([`Provider::refresh_body`], with the real refresh token) to the
/// provider's token endpoint with the proxy's own [`Upstream`], and store
/// a successful answer on the grant's current record. An answer with a
/// string in no known token format under a token-like key is refused (see
/// [`tokens::collect`]). A grant deleted while this ran (a sign-out
/// meanwhile) gets the new tokens revoked instead of stored.
async fn relay_one(
    provider: &'static dyn Provider,
    store: &Arc<TokenStore>,
    upstream: &Upstream,
    surrogate: &str,
) -> anyhow::Result<Relayed> {
    let service = provider.id();
    let formats = provider.formats();
    // The store as it is now: another process may already have rotated
    // the real refresh token.
    let Some(grant) = store
        .grant_of(service, surrogate, &[TokenKind::Refresh])
        .await?
    else {
        return Ok(Relayed::Unknown);
    };
    let Some(refresh_token) = grant.real(TokenKind::Refresh) else {
        return Ok(Relayed::Unknown);
    };
    let body = provider.refresh_body(&grant, refresh_token);
    let (status, bytes) = upstream.post_json(provider.token_path(), &body).await?;
    if !status.is_success() {
        return Ok(Relayed::Refused(relay_error(status, &bytes, formats)));
    }
    let Ok(Value::Object(answer)) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(Relayed::Refused(server_error(format_args!(
            "{}: a refresh answer that is no JSON object",
            service.name()
        ))));
    };
    let found = match tokens::collect(formats, &answer, OAUTH_KINDS, true) {
        Ok(found) => found,
        Err(refusal) => {
            return Ok(Relayed::Refused(server_error(format_args!(
                "{}: a refresh answer: {refusal}",
                service.name()
            ))));
        }
    };
    let new_access = found
        .iter()
        .find(|f| f.kind == TokenKind::Access)
        .map(|f| f.real.clone());
    let new_refresh = found
        .iter()
        .find(|f| f.kind == TokenKind::Refresh)
        .map(|f| f.real.clone());
    let scopes = scopes_of(&answer);
    let stored = store
        .update_grant(service, &grant.id, move |grant| {
            let surrogates = tokens::apply_refresh(grant, &found)?;
            if !scopes.is_empty() {
                grant.scopes = scopes;
            }
            Ok(surrogates)
        })
        .await?;
    let Some((_, surrogates)) = stored else {
        tracing::debug!(
            "{}: signed out during a refresh; revoking its tokens",
            service.name()
        );
        revoke_tokens(
            provider,
            upstream,
            &grant.client_id,
            new_access.as_deref(),
            new_refresh.as_deref(),
        )
        .await;
        return Ok(Relayed::Unknown);
    };
    tracing::debug!("{}: relayed a refresh", service.name());
    let mut answer = Value::Object(answer);
    tokens::substitute(&mut answer, &surrogates);
    Ok(Relayed::Answer(answer))
}

/// Revoke the real tokens upstream with `client_id`, best effort
/// (failures are logged): whichever of `access_token`/`refresh_token` are
/// `Some` — the refresh token first, then the access token where the
/// provider revokes access tokens ([`Provider::revokes_access_tokens`]) or
/// there is no refresh token to revoke instead.
async fn revoke_tokens(
    provider: &dyn Provider,
    upstream: &Upstream,
    client_id: &str,
    access_token: Option<&str>,
    refresh_token: Option<&str>,
) {
    let mut tokens = Vec::new();
    if let Some(refresh) = refresh_token {
        tokens.push((refresh, "refresh_token"));
    }
    if let Some(access) = access_token
        && (tokens.is_empty() || provider.revokes_access_tokens())
    {
        tokens.push((access, "access_token"));
    }
    let service = provider.id().name();
    for (token, hint) in tokens {
        let mut body = json!({ "token": token, "token_type_hint": hint });
        if provider.revoke_names_client(hint) {
            body["client_id"] = client_id.into();
        }
        match upstream.post_json(provider.revoke_path(), &body).await {
            Ok((status, _)) if status.is_success() => {}
            Ok((status, body)) => tracing::warn!(
                "{service}: the provider answered the revoke of a {hint} with {status} {}",
                error_code(&body).unwrap_or_default()
            ),
            Err(e) => tracing::warn!("{service}: revoke a {hint}: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    #[test]
    fn path_spellings_normalize_to_one_route() {
        for (raw, want) in [
            ("/v1/oauth/token", "/v1/oauth/token"),
            ("/v1/oauth/token/", "/v1/oauth/token"),
            ("//v1//oauth/token", "/v1/oauth/token"),
            ("/v1/oauth/%74oken", "/v1/oauth/token"),
            ("/V1/OAuth/Token", "/v1/oauth/token"),
            ("/v1/oauth/token;x=1", "/v1/oauth/token"),
            ("/v1/./oauth/x/../token", "/v1/oauth/token"),
            ("/v1/oauth%2ftoken", "/v1/oauth/token"),
            ("/", "/"),
        ] {
            assert_eq!(normalize_path(raw), want, "{raw}");
        }
    }

    #[test]
    fn json_or_form_with_duplicate_key_is_refused() {
        assert!(serde_json::from_str::<UniqueObject>(r#"{"a":1,"b":2}"#).is_ok());
        assert!(serde_json::from_str::<UniqueObject>(r#"{"a":1,"a":2}"#).is_err());
        assert!(serde_json::from_str::<UniqueObject>("[1]").is_err());
        assert!(unique_form(b"a=1&b=2").is_some());
        assert!(unique_form(b"a=1&a=2").is_none());
    }

    #[test]
    fn passed_keys_keep_identifiers_that_look_like_tokens_but_not_token_keys() {
        let formats = &super::super::openai::FORMATS;
        let jwt = format!(
            "{}.{}.sig",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(br#"{"iss":"https://auth.openai.com"}"#),
        );
        let answer = json!({ "device_auth_id": jwt, "user_code": "ABCD-1234", "interval": "5" });
        let passed = ["device_auth_id", "user_code"];
        assert_eq!(token_field(&answer, formats, &passed, "$"), None);
        assert_eq!(
            token_field(&answer, formats, &[], "$").as_deref(),
            Some("$.device_auth_id")
        );
        let pending = json!({ "error": { "code": "deviceauth_authorization_pending" } });
        assert_eq!(token_field(&pending, formats, &passed, "$"), None);
        let leak = json!({ "error": { "code": jwt.clone() } });
        assert_eq!(
            token_field(&leak, formats, &passed, "$").as_deref(),
            Some("$.error.code")
        );
        assert!(carries_token(&json!({ "code": "c" }), formats));
        let leak = json!({ "device_auth_id": "d", "x": [{ "access_token": "t" }] });
        assert_eq!(
            token_field(&leak, formats, &passed, "$").as_deref(),
            Some("$.x[0].access_token")
        );
    }
}
