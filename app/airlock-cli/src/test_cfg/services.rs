//! Helpers for the network services: grants in a store without a sign-in,
//! a guest for the sign-in forwards, and a direct call of the production
//! services where `next` is the upstream.

use std::cell::RefCell;
use std::ops::RangeInclusive;
use std::rc::Rc;
use std::sync::Arc;

use airlock_common::supervisor_capnp::supervisor;
use airlock_test_utils::tls_trusting;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Either, Full};
use hyper::{HeaderMap, Request, Response, StatusCode};

use super::context::{StoreHome, test_store};
use crate::network::http::{BoxError, ResponseBody};
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::{Endpoint, InjectedSecret};
use crate::project::MaskedSecret;
use crate::rpc::guest_network::GuestNetwork;
use crate::services::auth_codes::PendingCodes;
use crate::services::store::{NewGrant, TokenStore};
use crate::services::tokens::{Token, TokenKind};
use crate::services::{ServiceId, anthropic, openai};

/// A grant made without the proxy, with the account `acct`, the client
/// `client` and `tokens` as `(kind, real, surrogate)`.
pub fn new_grant(tokens: &[(TokenKind, &str, &str)]) -> NewGrant {
    NewGrant {
        account_id: "acct".into(),
        account: None,
        organization: None,
        client_id: "client".into(),
        scopes: vec![],
        tokens: tokens
            .iter()
            .map(|(kind, real, surrogate)| Token {
                kind: *kind,
                real: (*real).into(),
                surrogate: (*surrogate).into(),
                expires_at: None,
            })
            .collect(),
    }
}

/// The grant of a sign-in of `account`, with the client `client-1` and two
/// scopes. It has the access token `access` and the refresh token
/// `<access>-refresh`. Each surrogate is `<real>-surrogate`, and each token
/// is valid for one minute.
pub fn signed_in_grant(access: &str, account: &str) -> NewGrant {
    let token = |kind, real: String| Token {
        kind,
        surrogate: format!("{real}-surrogate"),
        real,
        expires_at: Some(crate::services::store::now_ms() + 60_000),
    };
    NewGrant {
        account_id: account.into(),
        account: Some(format!("{account}@example.com")),
        organization: Some("Org".into()),
        client_id: "client-1".into(),
        scopes: vec!["user:inference".into(), "user:profile".into()],
        tokens: vec![
            token(TokenKind::Access, access.into()),
            token(TokenKind::Refresh, format!("{access}-refresh")),
        ],
    }
}

/// Store a [`new_grant`] with `tokens` for `service`.
pub async fn insert_grant(
    store: &TokenStore,
    service: ServiceId,
    tokens: &[(TokenKind, &str, &str)],
) {
    store
        .insert_grant(service, new_grant(tokens))
        .await
        .unwrap();
}

/// A masked secret of the sandbox environment.
pub fn masked(name: &str, real: &str, surrogate: &str) -> MaskedSecret {
    MaskedSecret {
        name: name.into(),
        real: real.into(),
        surrogate: surrogate.into(),
    }
}

/// A real token with a realistic shape: `prefix`, a dash and 90
/// characters.
pub fn shaped_token(prefix: &str) -> String {
    format!("{prefix}-{}", "R".repeat(90))
}

/// A supervisor that implements nothing.
struct NoSupervisor;
impl supervisor::Server for NoSupervisor {}

/// A guest that no connection reaches. The accept loops of the sign-in
/// forwards call the guest only for a connection.
pub fn idle_guest() -> GuestNetwork {
    GuestNetwork::new(capnp_rpc::new_client(NoSupervisor))
}

/// A free loopback port in `range`.
pub fn free_port_in(range: RangeInclusive<u16>) -> u16 {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if range.contains(&port) {
            return port;
        }
    }
}

/// A free loopback port in the callback port range of Claude Code.
pub fn free_claude_callback_port() -> u16 {
    free_port_in(32768..=60999)
}

/// One request as `next` received it.
#[derive(Clone, Debug)]
pub struct Got {
    /// The request URI.
    pub uri: String,
    /// The request headers.
    pub headers: HeaderMap,
    /// The request body as text.
    pub body: String,
}

impl Got {
    /// The value of the header `name`, if it is valid text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

/// All requests that a `next` of [`answering`] received.
#[derive(Clone, Default)]
pub struct GotLog(Rc<RefCell<Vec<Got>>>);

impl GotLog {
    /// All requests, in order.
    pub fn all(&self) -> Vec<Got> {
        self.0.borrow().clone()
    }

    /// The number of requests.
    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }

    /// Whether there are no requests.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }
}

/// An upstream that records each request in `log`. It answers `200` with
/// `body` of the type `content_type` and a content length.
pub fn answering(log: &GotLog, content_type: &'static str, body: &str) -> Next {
    let (log, body) = (log.clone(), body.to_string());
    Box::new(move |req: Request<ResponseBody>| {
        Box::pin(async move {
            let (parts, req_body) = req.into_parts();
            let req_body = req_body.collect().await.unwrap().to_bytes();
            log.0.borrow_mut().push(Got {
                uri: parts.uri.to_string(),
                headers: parts.headers,
                body: String::from_utf8_lossy(&req_body).into_owned(),
            });
            let mut resp = Response::new(Either::Right(Full::new(Bytes::from(body.clone()))));
            resp.headers_mut()
                .insert("content-type", content_type.parse().unwrap());
            resp.headers_mut()
                .insert("content-length", body.len().to_string().parse().unwrap());
            Ok(resp)
        })
    })
}

/// An upstream that answers `200` with `frames` of the type
/// `content_type`. It streams them with no content length.
pub fn streaming_frames(
    frames: Vec<hyper::body::Frame<Bytes>>,
    content_type: &'static str,
) -> Next {
    Box::new(move |_req: Request<ResponseBody>| {
        Box::pin(async move {
            let stream = futures::stream::iter(frames.into_iter().map(Ok::<_, BoxError>));
            let body = http_body_util::StreamBody::new(stream).boxed_unsync();
            let mut resp = Response::new(Either::Left(Either::Right(body)));
            resp.headers_mut()
                .insert("content-type", content_type.parse().unwrap());
            Ok(resp)
        })
    })
}

/// [`streaming_frames`] with one data frame for each of `chunks`.
pub fn streaming(chunks: &[&str], content_type: &'static str) -> Next {
    let frames = chunks
        .iter()
        .map(|c| hyper::body::Frame::data(Bytes::from(c.to_string())))
        .collect();
    streaming_frames(frames, content_type)
}

/// A request in the form that the relay gives to a service.
pub fn request(
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Request<ResponseBody> {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(Either::Right(Full::new(Bytes::from(body.to_string()))))
        .unwrap()
}

/// An answer as the guest reads it, up to its end or an error.
#[derive(Debug)]
pub struct Answer {
    /// The response status.
    pub status: StatusCode,
    /// The body text that arrived before the end or the error.
    pub body: String,
    /// The body ended with an error.
    pub failed: bool,
}

impl Answer {
    /// Whether the answer is a refusal: a local `502`, or a stream that
    /// ended with an error.
    pub fn refused(&self) -> bool {
        self.status == StatusCode::BAD_GATEWAY || self.failed
    }

    /// The body as JSON. Panics if the body is not JSON.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

/// Read `resp` as the guest does.
pub async fn read_answer(resp: Response<ResponseBody>) -> Answer {
    let status = resp.status();
    let mut body = resp.into_body();
    let mut read = Vec::new();
    let mut failed = false;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            failed = true;
            break;
        };
        read.extend_from_slice(&frame.into_data().unwrap_or_default());
    }
    Answer {
        status,
        body: String::from_utf8_lossy(&read).into_owned(),
        failed,
    }
}

/// The production services with their production endpoints and a store
/// in a temporary home. Their requests go only to `next`.
pub struct ProductionServices {
    _home: StoreHome,
    /// The token store of the services.
    pub store: Arc<TokenStore>,
    anthropic: Rc<dyn Interceptor>,
    openai: Rc<dyn Interceptor>,
}

/// Make the [`ProductionServices`].
pub fn production_services() -> ProductionServices {
    let (home, store) = test_store();
    let tls = Arc::new(tls_trusting(""));
    let codes = PendingCodes::default();
    let anthropic = Rc::new(anthropic::Anthropic::new(
        anthropic::Endpoints::production(),
        store.clone(),
        tls.clone(),
        codes.clone(),
    ));
    let openai = Rc::new(openai::Openai::new(
        openai::Endpoints::production(),
        store.clone(),
        tls,
        codes,
    ));
    ProductionServices {
        _home: home,
        store,
        anthropic,
        openai,
    }
}

impl ProductionServices {
    /// Let `service` handle `req` to `host:443` with the `injected`
    /// secrets.
    /// Returns:
    ///   The answer as the guest reads it.
    pub async fn send(
        &self,
        service: ServiceId,
        host: &str,
        req: Request<ResponseBody>,
        injected: &[InjectedSecret],
        next: Next,
    ) -> Answer {
        let interceptor = match service {
            ServiceId::Anthropic => &self.anthropic,
            ServiceId::Openai => &self.openai,
        };
        let resp = interceptor
            .send(&Endpoint::new(host, 443), req, injected, next)
            .await
            .unwrap();
        read_answer(resp).await
    }
}
