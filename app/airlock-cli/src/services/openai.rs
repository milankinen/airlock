//! The `openai` network service.
//!
//! Supports the ChatGPT sign-in of Codex (`codex login`) and its use of
//! chatgpt.com.
//!
//! The protocol details agree with Codex 0.158.0.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::LocalBoxFuture;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Map, Value, json};

use super::ServiceId;
use super::auth_codes::{Channel, PendingCodes};
use super::oauth::{self, Account, Grants, Provider, TokenRequest, Upstream};
use super::sign_in::SignInPage;
use super::store::{Grant, TokenStore};
use super::tokens::{self, FAKE_JWT_PREFIX, Format, Formats, TokenKind};
use crate::network::http::ResponseBody;
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::{Endpoint, InjectedSecret, NetworkTarget};

const TOKEN_PATH: &str = "/oauth/token";
const REVOKE_PATH: &str = "/oauth/revoke";
const DEVICE_USERCODE_PATH: &str = "/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_PATH: &str = "/api/accounts/deviceauth/token";
/// Identifiers of the device-code sign-in that Codex uses to poll
/// (`deviceauth/usercode` answers with them). They can look like tokens (a
/// JWT of OpenAI's issuer). But they have no value without the user's
/// approval, and the proxy replaces the code that the poll then gets.
const DEVICE_IDS: &[&str] = &["device_auth_id", "user_code"];
/// The `redirect_uri` of the device-code sign-in's exchange.
const DEVICE_REDIRECT: &str = "https://auth.openai.com/deviceauth/callback";

const REFRESH_PREFIX: &str = "airlock-rt-";
const ACCESS_PREFIX: &str = "airlock-at-";
const ID_PREFIX: &str = "airlock-id-";

/// The claim of OpenAI's real access and ID tokens that holds the ChatGPT
/// account, and the issuer of these tokens.
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const ISSUER: &str = "https://auth.openai.com";

/// Pages that a sign-in callback can send the browser to: the authorize
/// host and ChatGPT. Codex's own success page is on the loopback origin.
const PAGES: &[&str] = &["auth.openai.com", "chatgpt.com"];

/// OpenAI's token formats.
///
/// Access and ID tokens that are JWTs with an OpenAI claim get fake JWT
/// surrogates. The fake JWTs have the claims of the real tokens (also
/// `exp`), a random nonce and a random signature. Codex checks no JWT
/// signature. Any other value of the `access_token`, `refresh_token` or
/// `id_token` of a token answer gets an opaque surrogate (`airlock-at-…`,
/// `airlock-rt-…`, `airlock-id-…`). The key decides, as the agent reads
/// it, because the format of the refresh token is not known.
pub static FORMATS: Formats = Formats(&[
    Format {
        kind: TokenKind::Id,
        recognize: |k, v| k == "id_token" && is_openai_jwt(v),
        is_surrogate: is_fake_jwt,
        mint: mint_fake_jwt,
        starts: &["eyJ"],
        shape: |run| tokens::jwt_at_start(run).is_some_and(is_openai_jwt),
        carries_claims: true,
    },
    Format {
        kind: TokenKind::Access,
        recognize: |k, v| k != "id_token" && is_openai_jwt(v),
        is_surrogate: is_fake_jwt,
        mint: mint_fake_jwt,
        starts: &["eyJ"],
        shape: |run| tokens::jwt_at_start(run).is_some_and(is_openai_jwt),
        carries_claims: true,
    },
    // By key: the answer's own token fields, whatever their format.
    Format {
        kind: TokenKind::Access,
        recognize: |k, v| k == "access_token" && !v.is_empty(),
        is_surrogate: |v| v.starts_with(ACCESS_PREFIX),
        mint: |_| tokens::surrogate(ACCESS_PREFIX),
        starts: &[],
        shape: |_| false,
        carries_claims: false,
    },
    Format {
        kind: TokenKind::Refresh,
        recognize: |k, v| k == "refresh_token" && !v.is_empty(),
        is_surrogate: |v| v.starts_with(REFRESH_PREFIX),
        mint: |_| tokens::surrogate(REFRESH_PREFIX),
        starts: &[],
        shape: |_| false,
        carries_claims: false,
    },
    Format {
        kind: TokenKind::Id,
        recognize: |k, v| k == "id_token" && !v.is_empty(),
        is_surrogate: |v| v.starts_with(ID_PREFIX),
        mint: |_| tokens::surrogate(ID_PREFIX),
        starts: &[],
        shape: |_| false,
        carries_claims: false,
    },
]);

/// Whether `v` is a fake JWT of airlock.
fn is_fake_jwt(v: &str) -> bool {
    v.starts_with(FAKE_JWT_PREFIX)
}

/// Whether `v` is a JWT with OpenAI's auth claim or issuer, and not a fake
/// JWT of airlock.
fn is_openai_jwt(v: &str) -> bool {
    !is_fake_jwt(v)
        && tokens::jwt_claims(v).is_some_and(|claims| {
            claims.get(AUTH_CLAIM).is_some()
                || claims.get("iss").and_then(Value::as_str) == Some(ISSUER)
        })
}

/// Make a fake JWT surrogate for the real JWT `real`.
fn mint_fake_jwt(real: &str) -> anyhow::Result<String> {
    tokens::fake_jwt(real)?.ok_or_else(|| anyhow::anyhow!("the token is no JWT"))
}

/// The hosts of the service.
pub struct Endpoints {
    /// Sign-in, token endpoint and revoke (`auth.openai.com`).
    pub auth: Endpoint,
    /// The ChatGPT backend that Codex uses (`chatgpt.com`).
    pub chatgpt: Endpoint,
}

impl Endpoints {
    /// The production hosts.
    pub fn production() -> Self {
        Self {
            auth: Endpoint::new("auth.openai.com", 443),
            chatgpt: Endpoint::new("chatgpt.com", 443),
        }
    }

    /// The network targets of the hosts.
    pub fn targets(&self) -> Vec<NetworkTarget> {
        crate::network::target::targets_of(&[&self.auth, &self.chatgpt])
    }
}

/// Get the sign-in page. Codex listens on 127.0.0.1:1455, or on 1457 if
/// port 1455 is in use.
pub fn sign_in_pages() -> Vec<SignInPage> {
    vec![SignInPage {
        host: "auth.openai.com",
        path: "/oauth/authorize",
        callback_ports: &[1455..=1455, 1457..=1457],
        callback_path: "/auth/callback",
        pages: PAGES,
    }]
}

/// Exchange, refresh and revoke at OpenAI.
struct OpenaiOauth;

static PROVIDER: OpenaiOauth = OpenaiOauth;

impl Provider for OpenaiOauth {
    fn id(&self) -> ServiceId {
        ServiceId::Openai
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

    /// All three tokens, because Codex needs them.
    fn exchange_requires(&self) -> &'static [TokenKind] {
        &[TokenKind::Access, TokenKind::Refresh, TokenKind::Id]
    }

    /// Get the ChatGPT account (else the subject) and the email address
    /// from the claims of the ID token.
    fn account(&self, answer: &Map<String, Value>) -> Account {
        let claims = answer
            .get("id_token")
            .and_then(Value::as_str)
            .and_then(tokens::jwt_claims);
        Account {
            id: claims.as_ref().and_then(account_id),
            email: claims.as_ref().and_then(email),
            organization: None,
        }
    }

    /// Makes the refresh as Codex sends it: the grant's client id and no
    /// `scope`.
    fn refresh_body(&self, grant: &Grant, refresh_token: &str) -> Value {
        json!({
            "client_id": grant.client_id,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        })
    }

    /// The endpoint accepts both token kinds. Codex revokes the access
    /// token as a fallback.
    fn revokes_access_tokens(&self) -> bool {
        true
    }

    /// Codex's fallback revoke of the access token does not name the
    /// client.
    fn revoke_names_client(&self, hint: &str) -> bool {
        hint != "access_token"
    }
}

/// The interceptor of the `openai` service.
///
/// On the auth host (`auth.openai.com`), only these routes are served.
/// They match on the normalized path ([`oauth::normalize_path`]). Every
/// other route gets a local `403` ([`oauth::route_not_allowed`]).
///  * `POST /oauth/token` (parsed strictly, see [`TokenRequest`]):
///    - Form `authorization_code` (browser and device-code sign-in): the
///      `code` must be a surrogate code ([`super::auth_codes`]) issued to
///      this service through the channel of the `redirect_uri`. The
///      channel is the loopback callback port, or the device flow's
///      `https://auth.openai.com/deviceauth/callback`. The proxy puts the
///      real code in its place and forwards the exchange. The real
///      `id_token`, `access_token` and `refresh_token` of the answer are
///      stored as a new grant with the exchange's `client_id`. The guest
///      gets surrogates (see [`FORMATS`]).
///    - Form token exchange (an API key for the ID token): refused locally
///      with `unsupported_grant_type`. Codex accepts this as not fatal.
///    - JSON `refresh_token`: Codex's own refresh, relayed
///      ([`Grants::relay_refresh`]). An unknown refresh surrogate gets
///      `401 refresh_token_invalidated`. The fake JWTs are made again with
///      the new claims of the real tokens. The access surrogate that they
///      replace stays valid until its own `exp` (see
///      [`super::store::Grant::previous_access`]).
///    - Other grant types: refused locally (`unsupported_grant_type`).
///  * `POST /api/accounts/deviceauth/usercode` (device-code sign-in):
///    forwarded with that path and no query. Its answer goes through
///    [`oauth::backstop`].
///  * `POST /api/accounts/deviceauth/token` (device-code sign-in): the
///    `authorization_code` of the answer becomes a surrogate code.
///  * `POST /oauth/revoke`: a refresh or access surrogate deletes its
///    grant and revokes both real tokens upstream (refresh, then access).
///    The guest gets `200 {}`. `codex login` revokes before every sign-in,
///    so a new sign-in signs out the earlier grant (as on a host).
///
/// On chatgpt.com, every path is served (HTTP and the WebSocket upgrade),
/// with the credential swap of [`Grants::swap_headers`]. A 401 from
/// upstream passes through unchanged (Codex refreshes itself). Answers
/// stream through [`super::scan::scan_answer`]. The scan ends an answer
/// with a real access or ID token (a JWT with an OpenAI claim, not a fake
/// JWT) or with a real token of the store (also opaque tokens, and refresh
/// tokens of any format).
///
/// Any other host passes [`oauth::backstop`].
pub struct Openai {
    endpoints: Endpoints,
    grants: Grants,
    codes: PendingCodes,
    targets: Vec<NetworkTarget>,
}

impl Openai {
    /// Make the interceptor.
    /// Args:
    ///  - `endpoints`: The hosts of the service
    ///  - `store`: The shared token store
    ///  - `tls`: TLS config for the proxy's own calls to the auth host
    ///  - `codes`: Surrogate codes, shared with the service's sign-ins.
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
        let path = oauth::normalize_path(req.uri().path());
        if *to == self.endpoints.auth && req.method() == Method::POST {
            match path.as_str() {
                TOKEN_PATH => return self.token(req, next).await,
                REVOKE_PATH => {
                    return match TokenRequest::read(req).await? {
                        Ok((_, token)) => self.grants.revoke(token.field("token")).await,
                        Err(refused) => Ok(refused),
                    };
                }
                DEVICE_USERCODE_PATH => {
                    oauth::route_to(&mut req, DEVICE_USERCODE_PATH)?;
                    return oauth::forward_auth_host(req, next, &FORMATS, DEVICE_IDS).await;
                }
                DEVICE_TOKEN_PATH => {
                    oauth::route_to(&mut req, DEVICE_TOKEN_PATH)?;
                    return self.device_token(req, next).await;
                }
                _ => {}
            }
        }
        if *to == self.endpoints.chatgpt {
            return self
                .grants
                .forward_api(to, req, injected, next, sign_in_again)
                .await;
        }
        // Fail closed: the auth host serves only its routes. A host that
        // the service does not know goes through the backstop.
        if *to == self.endpoints.auth {
            return Ok(oauth::route_not_allowed(ServiceId::Openai, to, &req));
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
                    ServiceId::Openai,
                    None,
                    Some(DEVICE_REDIRECT),
                ) {
                    return Ok(oauth::token_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_grant",
                        format_args!("openai: {why}"),
                    ));
                }
                self.grants.exchange(parts, token, next).await
            }
            Some("refresh_token") => self.grants.relay_refresh(token, invalidated).await,
            // This includes the API-key exchange (token exchange). It is
            // never forwarded.
            other => Ok(oauth::token_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                format_args!("openai: a token request with grant_type {other:?}"),
            )),
        }
    }

    /// Handle the device-code poll. Its `authorization_code` goes to the
    /// guest as a surrogate code.
    async fn device_token(
        &self,
        req: Request<ResponseBody>,
        next: Next,
    ) -> anyhow::Result<Response<ResponseBody>> {
        let (parts, bytes) = oauth::forward_buffered(req, next).await?;
        if !parts.status.is_success() {
            return oauth::backstop(oauth::rebuilt(parts, bytes), &FORMATS, DEVICE_IDS).await;
        }
        let Some(mut answer) = oauth::answer_object(&parts, &bytes) else {
            return Ok(oauth::server_error(
                "openai: a device poll answer that is no uncompressed JSON object",
            ));
        };
        let Some(Value::String(code)) = answer.get("authorization_code") else {
            return Ok(oauth::server_error(
                "openai: a device poll answer without authorization_code",
            ));
        };
        let surrogate = self.codes.issue(code, ServiceId::Openai, Channel::Device)?;
        answer.insert("authorization_code".into(), surrogate.into());
        // The PKCE verifier goes with the surrogate code. Without the real
        // code, it has no value. A token in any other field refuses the
        // answer.
        let mut rest = answer.clone();
        rest.remove("code_verifier");
        if oauth::carries_token(&Value::Object(rest), &FORMATS) {
            return Ok(oauth::server_error(
                "openai: a device poll answer that carries a token besides its code",
            ));
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

/// Get the ChatGPT account from ID-token claims (else the subject).
fn account_id(claims: &Value) -> Option<String> {
    claims
        .get(AUTH_CLAIM)
        .and_then(|a| a.get("chatgpt_account_id"))
        .or_else(|| claims.get("sub"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(String::from)
}

/// Get the email address from ID-token claims.
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

/// Make the answer for a refresh with a surrogate that airlock does not
/// know (signed out, never issued, or no refresh token).
fn invalidated() -> Response<ResponseBody> {
    oauth::token_error(
        StatusCode::UNAUTHORIZED,
        "refresh_token_invalidated",
        "openai: a refresh with a refresh token that is no surrogate of a stored sign-in",
    )
}

/// Make the answer for an API request with an unknown sign-in.
fn sign_in_again() -> Response<ResponseBody> {
    tracing::warn!(
        "refused: openai: an API request with a surrogate of no stored sign-in (answered 401)"
    );
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
    //! OpenAI token formats: real JWTs, surrogates and token kinds of the
    //! token answer.

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    /// A JWT-like string with `claims` and a fake signature.
    fn jwt(claims: &Value) -> String {
        format!(
            "eyJhbGciOiJSUzI1NiJ9.{}.sig",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    /// Test that only an OpenAI JWT counts as a real token. Fake JWT
    /// surrogates and other strings must not count as real.
    ///   1. Check that an access token with the auth claim and an ID token
    ///      from the OpenAI issuer are real
    ///   2. Check that a fake JWT, a JWT with other claims, surrogates, refresh
    ///      token shapes and tokens of other providers are not real
    #[test]
    fn openai_jwt_is_real_and_surrogates_or_other_strings_are_not() {
        let access = jwt(&json!({ "exp": 1, AUTH_CLAIM: { "chatgpt_plan_type": "plus" } }));
        let id_token = jwt(&json!({ "exp": 1, "iss": ISSUER }));
        let fake = tokens::fake_jwt(&access).unwrap().unwrap();
        for real in [&access, &id_token] {
            assert!(FORMATS.is_real(real), "{real}");
        }
        for other in [
            fake.as_str(),
            &jwt(&json!({ "sub": "x" })),
            "airlock-rt-abcdefghijklmnopqrstuvwxyz0123456789",
            "airlock-at-x",
            "rt_short",
            &format!("rt_{}", "a".repeat(40)),
            "sk-ant-oat01-x",
            "a.b.c",
        ] {
            assert!(!FORMATS.is_real(other), "{other}");
        }
    }

    /// Test that a string of a token answer gets its token kind from its key
    /// and value, and that only OpenAI JWTs carry claims.
    ///   1. Recognize JWT and opaque values under the token keys
    ///   2. Check the token kind and the claims flag of each
    ///   3. Check that an unknown key and an empty refresh token give nothing
    #[test]
    fn token_answer_string_gets_format_by_key_and_value() {
        let access = jwt(&json!({ "exp": 1, AUTH_CLAIM: {} }));
        let kind = |k: &str, v: &str| FORMATS.recognize(k, v).map(|f| (f.kind, f.carries_claims));
        assert_eq!(kind("id_token", &access), Some((TokenKind::Id, true)));
        assert_eq!(
            kind("access_token", &access),
            Some((TokenKind::Access, true))
        );
        assert_eq!(
            kind("access_token", "opaque-access-token-123"),
            Some((TokenKind::Access, false))
        );
        assert_eq!(kind("other", "opaque-access-token-123"), None);
        assert_eq!(
            kind("access_token", "short"),
            Some((TokenKind::Access, false))
        );
        assert_eq!(
            kind("refresh_token", "v1.anything"),
            Some((TokenKind::Refresh, false))
        );
        assert_eq!(kind("id_token", "opaque"), Some((TokenKind::Id, false)));
        assert_eq!(kind("refresh_token", ""), None);
    }
}
