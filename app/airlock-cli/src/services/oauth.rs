//! OAuth 2 logic shared by the services: the proxy's own HTTPS client for
//! token endpoints, strict parsing of token requests, checks of token
//! answers, surrogate tokens, the token swap on API requests, and the
//! relay of the agent's own refresh and revoke of grants.
//!
//! Provider specifics (endpoints, request shapes, error codes) stay in the
//! provider modules behind [`Provider`].
//!
//! ## Fail closed
//!
//! - A token request (exchange, refresh) must have no query, a JSON or
//!   form body without duplicate keys, and a grant type of the provider's
//!   list; anything else gets a local error and is never forwarded. What
//!   goes upstream is re-serialized from the parsed fields, never the
//!   guest's bytes.
//! - A 2xx token answer must be an uncompressed JSON object with the
//!   expected fields, else the guest gets a local `502 server_error`.
//! - Every other answer on an owned host's non-API paths (the auth hosts
//!   `platform.claude.com` and `auth.openai.com`, chatgpt.com outside
//!   `/backend-api/`) passes [`backstop`]: a JSON answer up to
//!   [`BODY_LIMIT`] that carries a token field (`access_token`,
//!   `refresh_token`, `id_token`), a code field (`authorization_code`,
//!   `code`, `code_verifier`; a surrogate code airlock put there does not
//!   count) or a string in a provider token format is refused. Larger or
//!   non-JSON answers pass unchanged: the providers issue their tokens in
//!   JSON token answers, which the services handle themselves.
//! - API answers pass [`api_backstop`]: an uncompressed JSON answer with a
//!   `Content-Length` up to [`BODY_LIMIT`] that holds a real token in an
//!   Anthropic format (`sk-ant-api`, `sk-ant-oat`, `sk-ant-ort`, without
//!   the `-airlock-` of a surrogate) is refused. Streams (SSE), compressed
//!   answers and answers without a length pass unbuffered. A 401 from the
//!   API passes through unchanged: refreshing is the agent's job, as on a
//!   host.
//! - The API credentials are strict ([`Grants::api_credential`]): a
//!   surrogate of a grant, or a masked secret airlock injected; an unknown
//!   surrogate gets the provider's "sign in again", anything else
//!   [`foreign_credential`]. Neither goes upstream.
//! - The authority of every request is the endpoint's
//!   ([`pin_authority`]).
//!
//! ## Refresh relay
//!
//! The agent refreshes its own tokens; the proxy only relays that call
//! ([`Grants::relay_refresh`]): look up the grant by its refresh
//! surrogate, read its real refresh token fresh (never a cached copy:
//! another process may already have rotated it), forward the request
//! upstream with the proxy's own [`Upstream`] (never the guest's
//! connection) in a task of its own — a dropped guest request does not
//! lose the rotated tokens — and store the answer with
//! [`super::store::TokenStore::replace_tokens`] in one transaction on the
//! grant's current record. A sign-out that finishes while the refresh is
//! in flight leaves no record to store on: the new tokens are revoked
//! instead. The guest gets surrogates with the upstream's own `expires_in`
//! / real `exp` — never a minimum or a synthetic one. Races between two
//! refreshes of the same grant (two sandboxes, a retry) are the agent's
//! problem, as they would be on a host; the store only guarantees that
//! each write is atomic.
//!
//! ## Revoke
//!
//! A sign-out with either surrogate deletes the grant and revokes its
//! real refresh token, then its access token where the provider revokes
//! access tokens ([`Provider::revokes_access_tokens`]). A new sign-in that
//! replaces older grants revokes them too, in the background.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Either, Full, Limited};
use hyper::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
    HeaderMap, HeaderValue, TRANSFER_ENCODING, USER_AGENT,
};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde_json::{Map, Value, json};

use super::ServiceId;
use super::auth_codes::{Channel, PREFIX, PendingCodes};
use super::store::{Grant, GrantSecrets, NewGrant, SurrogateKind, TokenStore};
use crate::network::http::ResponseBody;
use crate::network::interceptor::Next;
use crate::network::target::{Endpoint, InjectedSecret};

/// Cap on token-endpoint bodies the proxy reads (both directions).
pub const BODY_LIMIT: usize = 64 * 1024;

/// Bound on one call of the proxy to a token endpoint.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);

/// What every fake JWT starts with: its fixed header
/// `{"alg":"none","typ":"JWT"}` (base64url) and the dot. Real tokens never
/// use `alg` `none`.
pub const FAKE_JWT_PREFIX: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.";

/// Keys of a JSON answer that carry tokens.
const TOKEN_KEYS: &[&str] = &["access_token", "refresh_token", "id_token"];

/// Keys of a JSON answer that carry an authorization code or its PKCE
/// verifier. A code value that is a surrogate code
/// ([`super::auth_codes::PREFIX`]) does not count: airlock put it there.
const CODE_KEYS: &[&str] = &["authorization_code", "code", "code_verifier"];

/// What every surrogate in a provider token format carries: a real token
/// with it is not issued by the provider.
const SURROGATE_MARK: &str = "-airlock-";

/// What the strings of real Anthropic API keys and OAuth tokens start with.
pub const TOKEN_FORMATS: &[&str] = &["sk-ant-api", "sk-ant-oat", "sk-ant-ort"];

/// What a provider adds to the shared refresh relay and revoke. `Sync`:
/// [`Grants::relay_refresh`] moves the `&'static dyn Provider` into a
/// task of its own.
pub trait Provider: Sync {
    fn id(&self) -> ServiceId;

    /// Path of the token endpoint (refresh).
    fn token_path(&self) -> &'static str;

    /// Path of the revoke endpoint.
    fn revoke_path(&self) -> &'static str;

    /// The JSON body of a revoke of the real `token` (`hint`:
    /// `access_token` or `refresh_token`).
    fn revoke_body(&self, token: &str, hint: &str) -> Value;

    /// Whether the revoke endpoint takes access tokens too (else a
    /// sign-out revokes the refresh token only).
    fn revokes_access_tokens(&self) -> bool;

    /// Apply a successful upstream refresh `answer` to `secrets` in
    /// place: the real tokens, their expiry, and any surrogate the
    /// refresh re-mints (keeping the one it replaces working through
    /// [`super::store::Surrogates::keep_previous_access`] where that
    /// applies).
    fn apply_refresh(
        &self,
        secrets: &mut GrantSecrets,
        answer: &Map<String, Value>,
    ) -> anyhow::Result<()>;

    /// The guest's refresh answer: `answer` (the upstream's fields, the
    /// real tokens already removed) with `grant`'s current surrogates
    /// put back in their place. Leaves `expires_in` as the upstream sent
    /// it.
    fn surrogate_answer(&self, grant: &Grant, answer: Map<String, Value>) -> Map<String, Value>;
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

/// A token-endpoint error answered by the proxy: `{"error": code}`.
pub fn token_error(status: StatusCode, code: &str) -> Response<ResponseBody> {
    json_response(status, &json!({ "error": code }))
}

/// The answer to a token answer the proxy refuses to pass on.
pub fn server_error() -> Response<ResponseBody> {
    token_error(StatusCode::BAD_GATEWAY, "server_error")
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
/// field (unless it holds a surrogate code, see [`CODE_KEYS`]), or a
/// string that starts with one of `formats`.
pub fn carries_token(value: &Value, formats: &[&str]) -> bool {
    match value {
        Value::String(s) => formats.iter().any(|p| s.starts_with(p)),
        Value::Array(items) => items.iter().any(|v| carries_token(v, formats)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            let k = k.as_str();
            let surrogate_code = matches!(v, Value::String(c) if c.starts_with(PREFIX));
            TOKEN_KEYS.contains(&k)
                || (CODE_KEYS.contains(&k) && !surrogate_code)
                || carries_token(v, formats)
        }),
        _ => false,
    }
}

/// Whether a JSON value holds a string in a provider token format
/// ([`TOKEN_FORMATS`]) that airlock did not issue (no [`SURROGATE_MARK`]).
fn carries_real_format(value: &Value) -> bool {
    match value {
        Value::String(s) => {
            TOKEN_FORMATS.iter().any(|p| s.starts_with(p)) && !s.contains(SURROGATE_MARK)
        }
        Value::Array(items) => items.iter().any(carries_real_format),
        Value::Object(map) => map.values().any(carries_real_format),
        _ => false,
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

/// Forward a request to an owned host that no service rule handles (an
/// auth host, a non-API path, a host the service does not know), asking
/// for an uncompressed answer, and pass the answer through [`backstop`].
/// Never with a token swap.
pub async fn forward_auth_host(
    mut req: Request<ResponseBody>,
    next: Next,
) -> anyhow::Result<Response<ResponseBody>> {
    req.headers_mut()
        .insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    backstop(next(req).await?).await
}

/// The last check of an answer on an owned host's non-API paths (see the
/// module docs): a JSON answer up to [`BODY_LIMIT`] is read and refused
/// when it [`carries_token`]; JSON that is compressed (the proxy asked for
/// none) is refused unread. Larger or non-JSON answers pass unchanged.
pub async fn backstop(resp: Response<ResponseBody>) -> anyhow::Result<Response<ResponseBody>> {
    if !is_json(resp.headers()) || content_length(resp.headers()).is_some_and(|n| n > BODY_LIMIT) {
        return Ok(resp);
    }
    if is_encoded(resp.headers()) {
        tracing::warn!("refused a compressed JSON answer of an auth host");
        return Ok(server_error());
    }
    let (parts, body) = resp.into_parts();
    let body = read_body(body).await?;
    if serde_json::from_slice::<Value>(&body).is_ok_and(|v| carries_token(&v, TOKEN_FORMATS)) {
        tracing::warn!("refused an auth host answer that carries a token");
        return Ok(server_error());
    }
    Ok(rebuilt(parts, body))
}

/// The last check of an answer on an API path: an uncompressed JSON
/// answer with a `Content-Length` up to [`BODY_LIMIT`] is read and refused
/// when it holds a real token in a provider format
/// ([`carries_real_format`]). Everything else passes untouched and
/// unbuffered: streams (SSE), compressed answers, answers without a
/// length.
pub async fn api_backstop(resp: Response<ResponseBody>) -> anyhow::Result<Response<ResponseBody>> {
    let small = content_length(resp.headers()).is_some_and(|n| n <= BODY_LIMIT);
    if !is_json(resp.headers()) || !small || is_encoded(resp.headers()) {
        return Ok(resp);
    }
    let (parts, body) = resp.into_parts();
    let body = read_body(body).await?;
    if serde_json::from_slice::<Value>(&body).is_ok_and(|v| carries_real_format(&v)) {
        tracing::warn!("refused an API answer that carries a token airlock did not issue");
        return Ok(server_error());
    }
    Ok(rebuilt(parts, body))
}

/// The `Content-Type` is JSON (`application/json` or `+json`).
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
        .is_some_and(|mime| mime == "application/json" || mime.ends_with("+json"))
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

/// A random surrogate: `prefix` and 48 bytes from the CSPRNG, base64url.
pub fn surrogate(prefix: &str) -> anyhow::Result<String> {
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(super::store::random_bytes::<48>()?)
    ))
}

/// A random account id for a sign-in whose answer names no account: such
/// a grant never replaces another.
pub fn random_account_id() -> anyhow::Result<String> {
    Ok(format!(
        "airlock-random-{}",
        hex::encode(super::store::random_bytes::<16>()?)
    ))
}

/// The token of an `Authorization: Bearer <token>` header.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

/// Put the real access token of `grant` into `Authorization`.
pub fn set_bearer(headers: &mut HeaderMap, grant: &Grant) -> anyhow::Result<()> {
    let value = HeaderValue::from_str(&format!("Bearer {}", grant.secrets.access_token))
        .map_err(|_| anyhow!("the stored access token is not a valid header value"))?;
    headers.insert(AUTHORIZATION, value);
    Ok(())
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
        let invalid = || Ok(Err(token_error(StatusCode::BAD_REQUEST, "invalid_request")));
        if req.uri().query().is_some() || is_encoded(req.headers()) {
            return invalid();
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
            _ => return invalid(),
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
            return invalid();
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
        parts.uri = path.parse().context("token endpoint path")?;
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
/// whose code the user brings by hand (it is real already and passes
/// unchanged).
pub fn swap_code(
    token: &mut TokenRequest,
    codes: &PendingCodes,
    service: ServiceId,
    manual_redirect: Option<&str>,
    device_redirect: Option<&str>,
) -> bool {
    let (Some(code), Some(redirect)) = (token.field("code"), token.field("redirect_uri")) else {
        return false;
    };
    if manual_redirect == Some(redirect) {
        return !code.starts_with(PREFIX);
    }
    let Some(channel) = Channel::of_redirect(redirect, device_redirect) else {
        return false;
    };
    let Some(real) = codes.redeem(code, service, channel) else {
        return false;
    };
    token.fields.insert("code".into(), Value::String(real));
    true
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
fn relay_error(status: StatusCode, bytes: &[u8]) -> Response<ResponseBody> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) if !carries_token(&value, TOKEN_FORMATS) => json_response(status, &value),
        _ => server_error(),
    }
}

/// Who an API request's credential belongs to.
pub enum Credential {
    /// The request names no credential.
    None,
    /// A surrogate of this grant: it gets the real token.
    Grant(Box<Grant>),
    /// A masked secret the inject rules put in: passes unchanged.
    Injected,
}

/// The grant lifecycle of one service in this process: the token swap on
/// API requests, the refresh relay, and revoke.
pub struct Grants {
    provider: &'static dyn Provider,
    pub store: Arc<TokenStore>,
    upstream: Upstream,
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
        }
    }

    /// The grant of the access surrogate in `Authorization: Bearer`.
    /// `Ok(None)`: the header holds no surrogate airlock knows. Values
    /// without `is_surrogate` are not looked up.
    pub async fn bearer_grant(
        &self,
        headers: &HeaderMap,
        is_surrogate: fn(&str) -> bool,
    ) -> anyhow::Result<Option<Grant>> {
        let Some(token) = bearer(headers).filter(|t| is_surrogate(t)) else {
            return Ok(None);
        };
        self.store
            .find_by_surrogate(self.provider.id(), SurrogateKind::Access, token)
            .await
    }

    /// The strict check of an API request's `Authorization` header (see
    /// the module docs): an access surrogate of a grant, or a value of
    /// `injected`. `Err` holds the local answer: `sign_in_again` for a
    /// surrogate airlock does not know (signed out, or never issued),
    /// [`foreign_credential`] for any other value.
    pub async fn api_credential(
        &self,
        to: &Endpoint,
        headers: &HeaderMap,
        injected: &[InjectedSecret],
        is_surrogate: fn(&str) -> bool,
        sign_in_again: fn() -> Response<ResponseBody>,
    ) -> anyhow::Result<Result<Credential, Response<ResponseBody>>> {
        let Some(value) = headers.get(AUTHORIZATION) else {
            return Ok(Ok(Credential::None));
        };
        let value = value.to_str().unwrap_or_default();
        let token = bearer(headers);
        if token.is_some_and(is_surrogate) {
            return match self.bearer_grant(headers, is_surrogate).await? {
                Some(grant) => Ok(Ok(Credential::Grant(Box::new(grant)))),
                None => Ok(Err(sign_in_again())),
            };
        }
        if is_injected(value, injected) || token.is_some_and(|t| is_injected(t, injected)) {
            return Ok(Ok(Credential::Injected));
        }
        Ok(Err(foreign_credential(self.provider.id(), to)))
    }

    /// An API request: the credential passes [`Self::api_credential`]; an
    /// access surrogate gets the real token. The answer (a 401 included:
    /// refreshing is the agent's job) passes [`api_backstop`].
    pub async fn forward_api(
        &self,
        to: &Endpoint,
        mut req: Request<ResponseBody>,
        injected: &[InjectedSecret],
        next: Next,
        is_surrogate: fn(&str) -> bool,
        sign_in_again: fn() -> Response<ResponseBody>,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let grant = match self
            .api_credential(to, req.headers(), injected, is_surrogate, sign_in_again)
            .await?
        {
            Ok(Credential::Grant(grant)) => Some(*grant),
            Ok(Credential::None | Credential::Injected) => None,
            Err(refused) => return Ok(refused),
        };
        if let Some(grant) = &grant {
            set_bearer(req.headers_mut(), grant)?;
        }
        api_backstop(next(req).await?).await
    }

    /// Store a new grant (see [`TokenStore::insert_grant`]); the grants it
    /// replaces are revoked upstream in the background, best effort.
    pub async fn insert_grant(&self, new: NewGrant) -> anyhow::Result<Grant> {
        let (grant, replaced) = self.store.insert_grant(new).await?;
        for secrets in replaced {
            let (provider, upstream) = (self.provider, self.upstream.clone());
            tokio::task::spawn_local(async move {
                revoke_tokens(
                    provider,
                    &upstream,
                    Some(&secrets.access_token),
                    secrets.refresh_token.as_deref(),
                )
                .await;
            });
        }
        Ok(grant)
    }

    /// A sign-out: when `token` is the refresh or access surrogate of a
    /// grant, delete the grant and revoke its real tokens upstream (see
    /// [`revoke_tokens`]). Runs in a task of its own, so a dropped guest
    /// request still completes it. Unknown tokens are ignored; the guest
    /// always gets `200 {}`.
    pub async fn revoke(
        &self,
        token: Option<&str>,
        is_surrogate: fn(&str) -> bool,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let done = || json_response(StatusCode::OK, &json!({}));
        let Some(token) = token.filter(|t| is_surrogate(t)) else {
            return Ok(done());
        };
        let service = self.provider.id();
        let mut found = None;
        for kind in [SurrogateKind::Refresh, SurrogateKind::Access] {
            if let Some(grant) = self.store.find_by_surrogate(service, kind, token).await? {
                found = Some(grant);
                break;
            }
        }
        let Some(grant) = found else {
            return Ok(done());
        };
        let provider = self.provider;
        let store = self.store.clone();
        let upstream = self.upstream.clone();
        let task = tokio::task::spawn_local(async move {
            if let Err(e) = store.delete_grant(&grant.id).await {
                tracing::warn!("{}: delete a sign-in: {e:#}", service.name());
            }
            revoke_tokens(
                provider,
                &upstream,
                Some(&grant.secrets.access_token),
                grant.secrets.refresh_token.as_deref(),
            )
            .await;
            tracing::debug!("{}: signed out", service.name());
        });
        task.await.context("revoke task")?;
        Ok(done())
    }

    /// Relay the agent's own refresh of the grant of the refresh surrogate
    /// in `token.fields["refresh_token"]` (see the module docs). A
    /// request with no `refresh_token` field at all is malformed (`400
    /// invalid_request`); `unknown()` answers one that has the field but
    /// not as a surrogate airlock knows (signed out, never issued, or a
    /// grant with no refresh token).
    pub async fn relay_refresh(
        &self,
        token: TokenRequest,
        is_refresh_surrogate: fn(&str) -> bool,
        unknown: fn() -> Response<ResponseBody>,
    ) -> anyhow::Result<Response<ResponseBody>> {
        if token.field("refresh_token").is_none() {
            return Ok(token_error(StatusCode::BAD_REQUEST, "invalid_request"));
        }
        let Some(surrogate) = token
            .field("refresh_token")
            .filter(|t| is_refresh_surrogate(t))
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
            relay_one(provider, &store, &upstream, &surrogate, token.fields).await
        });
        match task.await.context("refresh relay task")?? {
            Relayed::Unknown => Ok(unknown()),
            Relayed::Refused(resp) => Ok(resp),
            Relayed::Answer(answer) => Ok(json_response(StatusCode::OK, &Value::Object(answer))),
        }
    }
}

/// What relaying one refresh produced.
enum Relayed {
    /// No grant for the surrogate (deleted, or never issued).
    Unknown,
    /// The upstream refused the refresh; its answer, checked for a leaked
    /// token.
    Refused(Response<ResponseBody>),
    /// Stored; the guest's answer (surrogates in place of the real
    /// tokens).
    Answer(Map<String, Value>),
}

/// Look up the grant of `surrogate`, forward `fields` (the agent's own
/// refresh request, its `refresh_token` field swapped for the real one)
/// to the provider's token endpoint with the proxy's own [`Upstream`],
/// and store a successful answer on the grant's current record (see
/// [`super::store::TokenStore::replace_tokens`]). A grant deleted while
/// this ran (a sign-out meanwhile) gets the new tokens revoked instead of
/// stored.
async fn relay_one(
    provider: &'static dyn Provider,
    store: &Arc<TokenStore>,
    upstream: &Upstream,
    surrogate: &str,
    mut fields: Map<String, Value>,
) -> anyhow::Result<Relayed> {
    let Some(found) = store
        .find_by_surrogate(provider.id(), SurrogateKind::Refresh, surrogate)
        .await?
    else {
        return Ok(Relayed::Unknown);
    };
    // Never the cache: another process may already have rotated it.
    let Some(grant) = store.grant(&found.id, provider.id()).await? else {
        return Ok(Relayed::Unknown);
    };
    let Some(refresh_token) = grant.secrets.refresh_token.clone() else {
        return Ok(Relayed::Unknown);
    };
    fields.insert("refresh_token".into(), refresh_token.into());
    let (status, bytes) = upstream
        .post_json(provider.token_path(), &Value::Object(fields))
        .await?;
    if !status.is_success() {
        return Ok(Relayed::Refused(relay_error(status, &bytes)));
    }
    let Ok(Value::Object(answer)) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(Relayed::Refused(server_error()));
    };
    let new_access = answer
        .get("access_token")
        .and_then(Value::as_str)
        .map(String::from);
    let new_refresh = answer
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(String::from);
    let mut rest = answer.clone();
    for key in TOKEN_KEYS {
        rest.remove(*key);
    }
    if carries_token(&Value::Object(rest.clone()), TOKEN_FORMATS) {
        return Ok(Relayed::Refused(server_error()));
    }
    let stored = store
        .replace_tokens(&grant.id, provider.id(), move |secrets| {
            provider.apply_refresh(secrets, &answer)
        })
        .await?;
    let Some(grant) = stored else {
        tracing::debug!(
            "{}: signed out during a refresh; revoking its tokens",
            provider.id().name()
        );
        revoke_tokens(
            provider,
            upstream,
            new_access.as_deref(),
            new_refresh.as_deref(),
        )
        .await;
        return Ok(Relayed::Unknown);
    };
    tracing::debug!("{}: relayed a refresh", provider.id().name());
    Ok(Relayed::Answer(provider.surrogate_answer(&grant, rest)))
}

/// Revoke the real tokens upstream, best effort (failures are logged):
/// whichever of `access_token`/`refresh_token` are `Some` — the refresh
/// token first, then the access token where the provider revokes access
/// tokens ([`Provider::revokes_access_tokens`]) or there is no refresh
/// token to revoke instead.
async fn revoke_tokens(
    provider: &dyn Provider,
    upstream: &Upstream,
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
        match upstream
            .post_json(provider.revoke_path(), &provider.revoke_body(token, hint))
            .await
        {
            Ok((status, _)) if status.is_success() => {}
            Ok((status, body)) => tracing::warn!(
                "{service}: the provider answered the revoke of a {hint} with {status} {}",
                error_code(&body).unwrap_or_default()
            ),
            Err(e) => tracing::warn!("{service}: revoke a {hint}: {e:#}"),
        }
    }
}

/// A fake unpadded JWT with the claims of `real` (if it is a JWT), `exp`
/// far ahead and a random nonce, and a random signature. `None` when
/// `real` is not a JWT.
pub fn fake_jwt(real: &str, exp_secs: i64) -> anyhow::Result<Option<String>> {
    let Some(Value::Object(mut claims)) = jwt_claims(real) else {
        return Ok(None);
    };
    claims.insert("exp".into(), exp_secs.into());
    claims.insert(
        "airlock_nonce".into(),
        URL_SAFE_NO_PAD
            .encode(super::store::random_bytes::<16>()?)
            .into(),
    );
    let payload = URL_SAFE_NO_PAD.encode(Value::Object(claims).to_string());
    let signature = URL_SAFE_NO_PAD.encode(super::store::random_bytes::<32>()?);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized() {
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
    fn the_fake_jwt_prefix_is_its_header() {
        assert_eq!(
            FAKE_JWT_PREFIX,
            format!(
                "{}.",
                URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#)
            )
        );
    }

    #[test]
    fn duplicate_keys_are_refused() {
        assert!(serde_json::from_str::<UniqueObject>(r#"{"a":1,"b":2}"#).is_ok());
        assert!(serde_json::from_str::<UniqueObject>(r#"{"a":1,"a":2}"#).is_err());
        assert!(serde_json::from_str::<UniqueObject>("[1]").is_err());
        assert!(unique_form(b"a=1&b=2").is_some());
        assert!(unique_form(b"a=1&a=2").is_none());
    }

    #[test]
    fn tokens_are_found_anywhere_in_json() {
        for v in [
            json!({ "access_token": "x" }),
            json!({ "data": [{ "id_token": 1 }] }),
            json!({ "key": "sk-ant-api03-abc" }),
            json!(["sk-ant-oat01-x"]),
            json!({ "a": { "b": "sk-ant-ort01-y" } }),
        ] {
            assert!(carries_token(&v, TOKEN_FORMATS), "{v}");
        }
        assert!(!carries_token(
            &json!({ "user_code": "ABCD", "n": 3 }),
            TOKEN_FORMATS
        ));
    }
}
