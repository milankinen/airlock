//! OAuth 2 logic that the network services share.
//!
//! Controls the full lifecycle of a sign-in for one service:
//!  * the exchange of the authorization code for tokens
//!  * the swap of surrogates for real tokens in API requests
//!  * the relay of token refreshes that the agent starts
//!  * the revoke of tokens when the agent signs out
//!
//! Also routes the requests to the token hosts of the provider. All parts
//! fail closed: the proxy refuses an unknown request or answer locally.

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

/// Maximum size of a token-endpoint body that the proxy reads (in both
/// directions).
pub const BODY_LIMIT: usize = 64 * 1024;

/// Maximum duration of one proxy call to a token endpoint.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);

/// Keys of a JSON answer that hold tokens.
const TOKEN_KEYS: &[&str] = &["access_token", "refresh_token", "id_token"];

/// Keys of a JSON answer that hold an authorization code or its PKCE
/// verifier. A surrogate code value ([`super::auth_codes::PREFIX`]) does
/// not count, because airlock put it there.
const CODE_KEYS: &[&str] = &["authorization_code", "code", "code_verifier"];

/// The credential header other than `Authorization`.
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");

/// Token kinds that an OAuth token answer can contain.
const OAUTH_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::Refresh, TokenKind::Id];

/// Surrogate kinds that are valid as an API credential.
const CREDENTIAL_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::ApiKey];

/// Surrogate kinds that can sign out a grant.
const SIGN_OUT_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::Refresh];

/// Owner of a new grant, from the exchange answer.
pub struct Account {
    /// The provider's account id. `None` if the answer names no account.
    /// Then the grant replaces no other grant.
    pub id: Option<String>,
    /// The email address, for `airlock show`.
    pub email: Option<String>,
    /// The organization name, if the answer names one.
    pub organization: Option<String>,
}

/// Provider-specific parts of the shared exchange, refresh relay and
/// revoke.
///
/// These parts stay per provider, for these reasons:
///  * Hosts and token-host routes: the allowlist is the purpose.
///  * Token formats: to fail closed, the proxy must know a real token.
///  * Callback ports and paths of the loopback sign-in: no arbitrary host
///    ports.
///  * Device flow and manual sign-in redirects.
///  * Shape of the refresh request (Claude Code's refresh does not ask for
///    `org:create_api_key`).
///  * The `create_api_key` endpoint that creates secrets (rate limit).
///
/// The trait is `Sync`, so a `&'static dyn Provider` is `Send`. The tasks
/// of [`Grants`] that hold it are local tasks, which do not need this.
pub trait Provider: Sync {
    /// The service identity.
    fn id(&self) -> ServiceId;

    /// The provider's token formats.
    fn formats(&self) -> &'static Formats;

    /// Path of the token endpoint (exchange, refresh).
    fn token_path(&self) -> &'static str;

    /// Path of the revoke endpoint.
    fn revoke_path(&self) -> &'static str;

    /// The main tokens that an exchange answer must contain.
    fn exchange_requires(&self) -> &'static [TokenKind];

    /// Get the account of an exchange `answer`. The answer still contains
    /// the real tokens.
    fn account(&self, answer: &Map<String, Value>) -> Account;

    /// Make the JSON body of a refresh of `grant`.
    /// Args:
    ///  - `grant`: The grant to refresh
    ///  - `refresh_token`: The real refresh token of the grant.
    ///
    /// Returns:
    ///   A body made from the provider's own fields and the grant's stored
    ///   client id and scopes (an allowlist). Never from the guest's
    ///   request.
    fn refresh_body(&self, grant: &Grant, refresh_token: &str) -> Value;

    /// Whether the revoke endpoint also accepts access tokens. If not, a
    /// sign-out revokes only the refresh token.
    fn revokes_access_tokens(&self) -> bool;

    /// Whether the revoke of a token of type `hint` (`access_token` or
    /// `refresh_token`) names the client. This must match the agent's own
    /// revoke.
    fn revoke_names_client(&self, _hint: &str) -> bool {
        true
    }
}

/// The proxy's own HTTPS client for the token endpoint of a provider.
/// Uses the proxy's TLS client config (system roots).
#[derive(Clone)]
pub struct Upstream {
    tls: Arc<rustls::ClientConfig>,
    endpoint: Endpoint,
}

impl Upstream {
    /// Make a client for `endpoint` with the TLS config `tls`.
    pub fn new(tls: Arc<rustls::ClientConfig>, endpoint: Endpoint) -> Self {
        Self { tls, endpoint }
    }

    /// Send `POST path` with a JSON `body`, with a [`UPSTREAM_TIMEOUT`].
    /// Returns:
    ///   The status and the body (at most [`BODY_LIMIT`] bytes).
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

/// Read a full body of at most [`BODY_LIMIT`] bytes.
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

/// Make a local response with a JSON body.
pub fn json_response(status: StatusCode, value: &Value) -> Response<ResponseBody> {
    let mut resp = Response::new(Either::Right(Full::new(Bytes::from(value.to_string()))));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

/// Make a local token-endpoint error answer: `{"error": code}`.
/// Args:
///  - `status`: HTTP status of the answer
///  - `code`: OAuth error code
///  - `why`: What airlock refused and why, for the log warning. Never a
///    secret value. The warning helps to find the cause of a failed
///    sign-in.
pub fn token_error(
    status: StatusCode,
    code: &str,
    why: impl std::fmt::Display,
) -> Response<ResponseBody> {
    tracing::warn!("refused: {why} (answered {} {code})", status.as_u16());
    json_response(status, &json!({ "error": code }))
}

/// Make the local `502` answer for an answer (or request) that the proxy
/// refuses to forward. Logs `why` (see [`token_error`]).
pub fn server_error(why: impl std::fmt::Display) -> Response<ResponseBody> {
    token_error(StatusCode::BAD_GATEWAY, "server_error", why)
}

/// Make a response again from its parts and a new (or read) body.
pub fn rebuilt(mut parts: hyper::http::response::Parts, body: Bytes) -> Response<ResponseBody> {
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove(TRANSFER_ENCODING);
    Response::from_parts(parts, Either::Right(Full::new(body)))
}

/// Forward `req`, ask for an uncompressed answer, and read the full
/// response. Use this for answers that the proxy must parse.
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

/// Get the JSON object of a 2xx answer that the proxy must change.
/// Returns:
///   `None` if the answer is compressed or is not a JSON object. The
///   caller then answers with [`server_error`].
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

/// Whether the body has a `Content-Encoding` other than `identity`.
fn is_encoded(headers: &HeaderMap) -> bool {
    headers
        .get_all(CONTENT_ENCODING)
        .iter()
        .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
}

/// Whether a JSON value contains a token or a code: a token field, a code
/// field (unless it holds a surrogate code, see [`CODE_KEYS`]), or a real
/// token of `formats`.
pub fn carries_token(value: &Value, formats: &Formats) -> bool {
    token_field(value, formats, &[], "$").is_some()
}

/// Find where a JSON value contains a token or a code (see
/// [`carries_token`]).
/// Args:
///  - `value`: The JSON value to search
///  - `formats`: Token formats of the provider
///  - `passed`: Keys whose string values are not checked for real token
///    formats. These are flow identifiers that look like tokens, for
///    example `device_auth_id` of the device sign-in. Their key names
///    still count.
///  - `path`: JSON path of `value`.
///
/// Returns:
///   A path such as `$.account.id` for the log (never the value), or
///   `None` if there is no token.
fn token_field(value: &Value, formats: &Formats, passed: &[&str], path: &str) -> Option<String> {
    find_token(value, formats, passed, path, false)
}

/// Do the search of [`token_field`] below `path`.
///
/// `in_error` is true if the value is inside an `error` or `errors`
/// field. There, `code` is an error code (for example
/// `deviceauth_authorization_pending` of the device poll), not an
/// authorization code. Its value is still checked for token formats.
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

/// Set the authority of `req` to the endpoint `to` that the guest
/// connected to.
///
/// Call this before the service swaps or forwards anything. The upstream
/// sees `to`, whatever `Host`, absolute-form target or `:authority` the
/// guest sent.
pub fn pin_authority(req: &mut Request<ResponseBody>, to: &Endpoint) -> anyhow::Result<()> {
    let authority = to.authority();
    let path = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    // HTTP/2 requests get an `https` URI with the endpoint's authority (and
    // no `Host`). Others get the origin form and `Host`.
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

/// Forward a request of an allowed token-host route, or a request to a
/// host that the service does not know. Asks for an uncompressed answer
/// and sends the answer through [`backstop`]. Never swaps a token.
/// Args:
///  - `req`: The request to forward
///  - `next`: The next step that sends the request upstream
///  - `formats`: Token formats of the provider
///  - `passed`: Keys whose string values [`backstop`] does not check for
///    token formats.
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

/// Make the local `403` answer for a token-host route that no service
/// rule allows. The request is not forwarded. The log names the method and
/// path.
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

/// Do the last check of an answer of a token host.
///
/// The proxy reads a JSON answer of up to [`BODY_LIMIT`] bytes and refuses
/// it if it [`carries_token`]. This includes a token field, a code field
/// or a real token of the provider. The proxy refuses compressed JSON
/// without reading it (the proxy asked for no compression). Non-JSON
/// answers, and JSON answers with a `Content-Length` above the limit, pass
/// unchanged: the providers issue their tokens in JSON token answers,
/// which the services handle themselves. A larger JSON body without a
/// `Content-Length` gives an error.
/// Args:
///  - `resp`: The answer of the token host
///  - `formats`: Token formats of the provider
///  - `passed`: Keys whose string values are not checked for token
///    formats (see [`token_field`]).
///
/// Returns:
///   The answer, or a local `502` if it is refused.
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

/// Whether the `Content-Type` is JSON (`application/json` or `+json`).
fn is_json(headers: &HeaderMap) -> bool {
    mime_type(headers).is_some_and(|mime| mime == "application/json" || mime.ends_with("+json"))
}

/// Get the media type of the `Content-Type`, lowercase, without
/// parameters.
fn mime_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
}

/// Get the `Content-Length`, if it is one number.
fn content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
}

/// Whether `value` is the real value of a masked secret that the inject
/// rules put into this request. Then airlock injected it, so it belongs
/// to the user.
/// Args:
///  - `value`: A credential header value, or the token of a `Bearer`
///    value
///  - `injected`: Secrets that the inject rules put into the request.
pub fn is_injected(value: &str, injected: &[InjectedSecret]) -> bool {
    !value.is_empty() && injected.iter().any(|s| s.real == value)
}

/// Make the local `401` answer for an API request with a credential that
/// is not a surrogate of the service and not a masked secret that airlock
/// injected. The message names the two valid sources.
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

/// Make a random account id for a sign-in whose answer names no account.
/// A grant with such an id never replaces another grant.
pub fn random_account_id() -> anyhow::Result<String> {
    Ok(format!(
        "airlock-random-{}",
        hex::encode(random_bytes::<16>()?)
    ))
}

/// Get the token of an `Authorization: Bearer <token>` value.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Make the URI that an allowed route forwards: `uri` with the path
/// `path` and no query. Never the guest's spelling of the path. Keeps the
/// scheme and authority of an HTTP/2 request ([`pin_authority`]).
fn routed_uri(uri: &hyper::Uri, path: &str) -> anyhow::Result<hyper::Uri> {
    let uri = match (uri.scheme_str(), uri.authority()) {
        (Some(scheme), Some(authority)) => format!("{scheme}://{authority}{path}"),
        _ => path.to_string(),
    };
    uri.parse().context("route URI")
}

/// Set the URI of `req` to the route `path`, with no query.
///
/// An allowed route goes upstream with its own path, never the guest's
/// spelling of it.
pub fn route_to(req: &mut Request<ResponseBody>, path: &str) -> anyhow::Result<()> {
    *req.uri_mut() = routed_uri(req.uri(), path)?;
    Ok(())
}

/// Get the scopes of the `scope` field of a token answer.
pub fn scopes_of(answer: &Map<String, Value>) -> Vec<String> {
    answer
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Make a request path canonical for route matching.
///
/// The result is percent-decoded and ASCII lowercase. It has no `;`
/// parameters, no empty or `.` segments, no `..` segments (they are
/// applied) and no trailing `/`.
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

/// Decode `%XX` escapes. Invalid escapes stay unchanged.
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
    /// `application/json`.
    Json,
    /// `application/x-www-form-urlencoded`.
    Form,
}

/// A token request (exchange, refresh or revoke), parsed strictly.
///
/// A valid request has no query and an uncompressed JSON or form body
/// without duplicate keys. The proxy refuses all other requests locally
/// and never forwards them. The request that goes upstream is serialized
/// again from the parsed fields, never from the guest's bytes.
pub struct TokenRequest {
    /// The body format that the guest used.
    pub format: BodyFormat,
    /// The body fields.
    pub fields: Map<String, Value>,
}

impl TokenRequest {
    /// Read and parse `req`.
    /// Returns:
    ///   The request parts and the parsed request. The inner `Err` holds
    ///   the local answer if the request is refused.
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

    /// Get the string field `name`.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).and_then(Value::as_str)
    }

    /// Make the request to forward.
    /// Args:
    ///  - `parts`: Parts of the guest's request
    ///  - `path`: The route path to use, without a query.
    ///
    /// Returns:
    ///   A request with a body serialized from the fields, in the format
    ///   that the guest used.
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

/// Parse the fields of a form. Returns `None` if a key occurs more than
/// once.
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

/// Replace the surrogate code of an exchange with the real code.
/// Args:
///  - `token`: The exchange request
///  - `codes`: Surrogate codes and opened pages of the service
///  - `service`: The service of the exchange
///  - `manual_redirect`: The `redirect_uri` of a manual sign-in, where the
///    user brings the real code by hand
///  - `device_redirect`: The `redirect_uri` of the provider's device flow.
///
/// Returns:
///   `Err` with the reason if the exchange is refused: its code is not a
///   surrogate that this process issued to `service` through the channel
///   of its `redirect_uri` ([`Channel::of_redirect`]).
///
/// The real code of a manual sign-in passes unchanged, but only if the
/// exchange's `code_verifier` belongs to a sign-in page that the browser
/// bridge opened for `service` ([`PendingCodes::redeem_page`]). Thus the
/// sandbox cannot bring a code of a sign-in that the host never opened
/// (for example, of another account).
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

/// Get the error code of a token-endpoint error body: `{"error": "code"}`
/// or `{"error": {"code": "..."}}`.
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

/// Relay an upstream token-endpoint error to the guest unchanged, after a
/// check for a leaked real token. The check of [`backstop`] applies also
/// to an error answer.
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

/// Owner of the credential of an API request.
pub enum Credential {
    /// The request names no credential.
    None,
    /// A surrogate of the grant with this id. The real value replaced it.
    Grant(String),
    /// A masked secret that the inject rules put in. It passes unchanged.
    Injected,
}

/// Result of [`Grants::swap_headers`].
pub struct Swapped {
    /// Owner of the request's credential.
    pub credential: Credential,
    /// The store snapshot, if the swap read it.
    pub snapshot: Option<Arc<Snapshot>>,
}

/// The grant lifecycle of one service in this process: the code exchange,
/// the credential swap on API requests, the refresh relay, and revoke.
///
/// The agent owns the token lifecycle, as it does on a host. It refreshes
/// its own real access token. The proxy only relays that call and stores
/// the answer ([`Self::relay_refresh`]), and never refreshes on its own.
/// An API request after the real expiry gets the provider's answer (also
/// a 401). A sign-out or refresh in one process is visible to the other
/// processes at their next lookup.
pub struct Grants {
    provider: &'static dyn Provider,
    /// The shared token store.
    pub store: Arc<TokenStore>,
    upstream: Upstream,
    /// True after the warning that the store cannot be read is logged.
    store_warned: std::cell::Cell<bool>,
}

impl Grants {
    /// Make the grant lifecycle of `provider`, with its `store` and its
    /// token-endpoint client `upstream`.
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

    /// Replace the surrogate credential of an API request with its real
    /// value.
    ///
    /// Only `Authorization: Bearer <token>` and `x-api-key: <value>`
    /// change, and only if the full token or value is a known access or
    /// API-key surrogate of the service. An injected masked secret passes
    /// unchanged. All other headers keep their surrogates, because an
    /// upstream can reflect a header (an `Origin` into
    /// `Access-Control-Allow-Origin`, a value into an error message).
    /// Request bodies never change.
    ///
    /// The credentials are strict: any other value is refused. Thus one
    /// sandbox cannot put its own token in the shared credential files and
    /// make the requests of other sandboxes use it.
    /// Args:
    ///  - `to`: The API endpoint
    ///  - `headers`: Request headers, changed in place
    ///  - `injected`: Masked secrets that the inject rules put in
    ///  - `sign_in_again`: Makes the provider's "sign in again" answer.
    ///
    /// Returns:
    ///   The credential owner and the store snapshot, if read. The inner
    ///   `Err` holds the local answer: `sign_in_again()` for a surrogate
    ///   that airlock does not know (signed out, or never issued), or
    ///   [`foreign_credential`] for any other value. A refresh or ID-token
    ///   surrogate is not an API credential, so it also gets
    ///   [`foreign_credential`].
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
                // Read the store only for a credential with a surrogate
                // shape.
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

    /// Forward an API request with the credential swap of
    /// [`Self::swap_headers`].
    ///
    /// The request asks for an uncompressed answer, whatever the guest
    /// accepts, so that [`scan::scan_answer`] can scan the answer. A 401
    /// passes through unchanged: the refresh is the job of the agent, as on
    /// a host. The body goes upstream as the guest sent it.
    /// Args:
    ///  - `to`: The API endpoint
    ///  - `req`: The guest's request
    ///  - `injected`: Masked secrets that the inject rules put in
    ///  - `next`: The next step that sends the request upstream
    ///  - `sign_in_again`: Makes the provider's "sign in again" answer.
    ///
    /// Returns:
    ///   The scanned answer, or a local answer if the request is refused.
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

    /// Get the real values that an API answer must not contain
    /// ([`scan::known_reals`]): the values of the store and of the secrets
    /// injected into the request.
    ///
    /// Reads the store now if the swap did not read it.
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
                    // Fail closed: no scan, so no answer.
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

    /// Forward a code exchange, keep the real tokens as a new grant, and
    /// answer with surrogates.
    ///
    /// The exchange must name its `client_id`. The grant keeps it for
    /// refresh and revoke. A 2xx answer must be an uncompressed JSON object
    /// whose tokens all have a known format of the provider
    /// ([`super::tokens::collect`]). It must also contain the tokens that
    /// the provider requires ([`Provider::exchange_requires`]). Otherwise
    /// the guest gets a local `502 server_error` and nothing is stored.
    /// Args:
    ///  - `parts`: Parts of the guest's request
    ///  - `token`: The exchange, with its code already swapped (see
    ///    [`swap_code`])
    ///  - `next`: The next step that sends the request upstream.
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

    /// Store a new grant (see [`TokenStore::insert_grant`]). Revokes the
    /// grants that it replaces upstream in the background, best effort.
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

    /// Sign out the grant of `token`.
    ///
    /// If `token` is an access or refresh surrogate of a grant, delete the
    /// grant. Then revoke its real refresh token upstream. Also revoke its
    /// access token if the provider revokes access tokens
    /// ([`Provider::revokes_access_tokens`]) or if the grant has no refresh
    /// token. The revoke uses the grant's stored client id. Unknown tokens
    /// are ignored.
    /// Returns:
    ///   Always `200 {}`.
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
        // Use a separate task, so that the sign-out completes also if the
        // guest drops the request.
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

    /// Relay the agent's own refresh of a grant.
    ///
    /// The proxy sends a refresh that the provider builds
    /// ([`Provider::refresh_body`]) with its own [`Upstream`], never on the
    /// guest's connection. Nothing else of the guest's request goes
    /// upstream. The guest gets surrogates with the upstream's own
    /// `expires_in` or real `exp`, never a minimum or a synthetic value.
    ///
    /// Two refreshes of the same grant can race (two sandboxes, a retry).
    /// This is the agent's problem, as on a host. The store only makes sure
    /// that each write is atomic.
    /// Args:
    ///  - `token`: The refresh request, with the refresh surrogate in its
    ///    `refresh_token` field
    ///  - `unknown`: Makes the answer for a `refresh_token` that is not a
    ///    surrogate that airlock knows (signed out, never issued, or a
    ///    grant without a refresh token).
    ///
    /// Returns:
    ///   The answer with surrogates, the provider's refusal, or
    ///   `400 invalid_request` if the request has no `refresh_token` field.
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
        // Use a separate task, so that the rotated tokens are stored also
        // if the guest drops the request.
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

/// Result of one relayed refresh.
enum Relayed {
    /// No grant for the surrogate (deleted, or never issued).
    Unknown,
    /// The upstream refused the refresh, or the proxy refused its answer.
    /// Holds the guest's answer.
    Refused(Response<ResponseBody>),
    /// The new tokens are stored. Holds the guest's answer, with
    /// surrogates in place of the real tokens.
    Answer(Value),
}

/// Refresh the grant of `surrogate` at the provider and store the result.
///
/// Sends the provider's refresh ([`Provider::refresh_body`], with the real
/// refresh token) with the proxy's own [`Upstream`]. Stores a successful
/// answer on the grant's current record in one transaction
/// ([`tokens::apply_refresh`]). The proxy refuses an answer with a string
/// in no known token format under a token-like key (see
/// [`tokens::collect`]).
/// Args:
///  - `provider`: The provider of the grant
///  - `store`: The token store
///  - `upstream`: Client of the provider's token endpoint
///  - `surrogate`: The refresh surrogate from the guest.
async fn relay_one(
    provider: &'static dyn Provider,
    store: &Arc<TokenStore>,
    upstream: &Upstream,
    surrogate: &str,
) -> anyhow::Result<Relayed> {
    let service = provider.id();
    let formats = provider.formats();
    // Read the current grant. Another process can already have rotated
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
    // A sign-out that completed during the refresh left no record. Revoke
    // the new tokens instead.
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

/// Revoke the real tokens upstream with `client_id`, best effort. Logs
/// failures.
///
/// Revokes the refresh token first, if there is one. Then revokes the
/// access token if the provider revokes access tokens
/// ([`Provider::revokes_access_tokens`]) or if there is no refresh token.
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
    //! OAuth helpers: path normalization, refusal of duplicate keys and the
    //! search for tokens in JSON answers.

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    /// Test that different spellings of a path normalize to one route. A
    /// guest must not avoid a route match with a different spelling.
    ///   1. Normalize slashes, case, percent codes, parameters and dot segments
    ///   2. Check that each gives the canonical path
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

    /// Test that a JSON object or a form with a duplicate key is refused. Two
    /// parsers can choose different values of a duplicate key.
    ///   1. Parse JSON with unique keys, a duplicate key and a non-object
    ///   2. Parse forms with unique keys and a duplicate key
    ///   3. Check that only the inputs with unique keys are accepted
    #[test]
    fn json_or_form_with_duplicate_key_is_refused() {
        assert!(serde_json::from_str::<UniqueObject>(r#"{"a":1,"b":2}"#).is_ok());
        assert!(serde_json::from_str::<UniqueObject>(r#"{"a":1,"a":2}"#).is_err());
        assert!(serde_json::from_str::<UniqueObject>("[1]").is_err());
        assert!(unique_form(b"a=1&b=2").is_some());
        assert!(unique_form(b"a=1&a=2").is_none());
    }

    /// Test that passed keys can hold values that look like real tokens, but
    /// that token key names and token values in other keys are still found.
    /// The device sign-in sends a JWT-like `device_auth_id` that is not a
    /// token.
    ///   1. Check that a JWT in a passed key is not found, and is found when
    ///      the key is not passed
    ///   2. Check that a pending device answer has no token
    ///   3. Check that a JWT in an error code is found
    ///   4. Check that a code key and a nested token key are found
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
