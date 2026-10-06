//! The sign-in callback forward of a network service.
//!
//! A sign-in tool in the guest listens for the OAuth callback on a
//! loopback port. [`super::sign_in::LoopbackSignIn`] binds that port on the
//! host and serves it here, so the browser's redirect to
//! `http://localhost:<port>/…` reaches the tool. The forward is no raw
//! relay: it parses each HTTP request and swaps the `code` of its query
//! for a surrogate code ([`super::auth_codes`]), bound to the sign-in's
//! service and callback port, so the real authorization code of a sign-in
//! never reaches the guest. The guest is untrusted, and the browser holds
//! the user's sessions, so the forward also:
//!
//! - forwards `GET` only (anything else gets `405` from the host), and
//!   strips `Cookie` and `Authorization` from the browser's requests;
//! - passes only the guest's answer headers of [`ANSWER_HEADERS`] (and a
//!   checked `Location`), so no `Set-Cookie`, `Refresh`, CORS or
//!   `Clear-Site-Data` reaches the browser, and adds
//!   `Content-Security-Policy: sandbox; default-src 'none'`;
//! - lets a redirect (`3xx` with `Location`) of the guest lead only to the
//!   same loopback origin (host and port) or to an `https` page of the
//!   service ([`Callback::pages`]); any other redirect is replaced by a
//!   host page that says the sign-in finished;
//! - closes after [`FOLLOW_UP`]: once a request carried a `code`, the
//!   browser's follow-ups (a success page) reach the guest for that long.
//!   A forward that gets no `code` closes after [`UNUSED_LIMIT`], so the
//!   sandbox cannot hold a host port forever. A closed forward drops later
//!   connections, answers later requests with the host page, and frees its
//!   port. A new sign-in on the same port opens a forward that has not
//!   closed yet again ([`CallbackForward::reopen`]).
//!
//! A request that does not parse as HTTP/1 gets an error from the host
//! and never reaches the guest.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use http_body_util::{Either, Full};
use hyper::body::Incoming;
use hyper::header::{
    ALLOW, AUTHORIZATION, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    COOKIE, HOST, HeaderMap, HeaderName, HeaderValue, LOCATION,
};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinSet;
use tracing::{debug, warn};
use url::Url;

use super::ServiceId;
use super::auth_codes::{Channel, PendingCodes};
use crate::network::reverse_forward::{self, BoundForward};
use crate::rpc::guest_network::GuestNetwork;

/// How long the browser's follow-ups reach the guest after the first
/// request with a `code`.
const FOLLOW_UP: Duration = Duration::from_mins(1);

/// How long a forward waits for its first request with a `code`.
const UNUSED_LIMIT: Duration = Duration::from_mins(10);

/// The headers of the guest's answers that reach the browser. A
/// `Location` reaches it too, on a redirect that [`to_browser`] allows.
const ANSWER_HEADERS: [HeaderName; 3] = [CONTENT_TYPE, CONTENT_LENGTH, CACHE_CONTROL];

/// The policy of the callback answers the guest sends to the browser.
const CSP: &str = "sandbox; default-src 'none'";

/// The page the host shows instead of a guest answer it refuses.
const FINISHED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>airlock</title>\
    <p>Sign-in finished; return to the terminal.</p>\n";

/// The sign-in a callback forward serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Callback {
    /// The service whose sign-in page named the callback.
    pub service: ServiceId,
    /// The hosts of the service's pages a redirect may lead to (`https`).
    pub pages: &'static [&'static str],
}

/// The state of one callback forward, shared by its listeners and
/// connections. Cheap to clone.
#[derive(Clone)]
pub struct CallbackForward(Rc<ForwardState>);

/// Runtime state of a forward: a new sign-in on the same port reopens it
/// with its own callback, and the first code starts the follow-up time.
struct ForwardState {
    port: u16,
    codes: PendingCodes,
    callback: Cell<Callback>,
    /// When the forward opened, or a new sign-in reopened it.
    opened: Cell<Instant>,
    /// When the first request with a `code` came.
    code_seen: Cell<Option<Instant>>,
    /// The forward has closed for good: its port is (being) freed.
    ended: Cell<bool>,
}

impl CallbackForward {
    fn new(port: u16, callback: Callback, codes: PendingCodes) -> Self {
        Self(Rc::new(ForwardState {
            port,
            codes,
            callback: Cell::new(callback),
            opened: Cell::new(Instant::now()),
            code_seen: Cell::new(None),
            ended: Cell::new(false),
        }))
    }

    /// Open the forward again for a new sign-in of `callback`. `false`
    /// when it has closed for good: the sign-in needs a new forward.
    pub fn reopen(&self, callback: Callback) -> bool {
        if self.0.ended.get() {
            return false;
        }
        self.0.callback.set(callback);
        self.0.opened.set(Instant::now());
        self.0.code_seen.set(None);
        true
    }

    /// When the forward closes: [`FOLLOW_UP`] after the first code, else
    /// [`UNUSED_LIMIT`] after it opened.
    fn deadline(&self) -> Instant {
        match self.0.code_seen.get() {
            Some(seen) => seen + FOLLOW_UP,
            None => self.0.opened.get() + UNUSED_LIMIT,
        }
    }

    /// Whether requests still reach the guest.
    fn is_open(&self) -> bool {
        !self.0.ended.get() && Instant::now() < self.deadline()
    }

    /// Wait until the forward closes, and mark it closed for good.
    async fn closed(&self) {
        while self.is_open() {
            tokio::time::sleep_until(self.deadline().into()).await;
        }
        self.0.ended.set(true);
    }

    /// The origin the browser reached the forward at, from `Host`: a
    /// loopback name with the forward's port. Else `127.0.0.1` and the
    /// port.
    fn origin(&self, req: &Request<Incoming>) -> Url {
        let port = self.0.port;
        req.headers()
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| Url::parse(&format!("http://{h}/")).ok())
            .filter(|u| {
                matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                    && u.port() == Some(port)
            })
            .unwrap_or_else(|| {
                Url::parse(&format!("http://127.0.0.1:{port}/")).expect("a valid URL")
            })
    }
}

/// Serve the bound callback port `forward` for a sign-in of `callback`
/// (see the module docs): a task in `tasks` that runs the accept loops
/// until the forward closes and then frees the port, each connection
/// relayed to the same port on `guest`, every request's `code` swapped for
/// a surrogate kept in `codes`. Returns the forward's state.
pub fn serve(
    forward: BoundForward,
    guest: &GuestNetwork,
    tasks: &mut JoinSet<()>,
    codes: &PendingCodes,
    callback: Callback,
) -> CallbackForward {
    let state = CallbackForward::new(forward.guest_port(), callback, codes.clone());
    serve_until_closed(forward, guest, tasks, state.clone());
    state
}

/// Serve `forward` for `state` in a task of `tasks` until `state` closes;
/// then the port is freed.
fn serve_until_closed(
    forward: BoundForward,
    guest: &GuestNetwork,
    tasks: &mut JoinSet<()>,
    state: CallbackForward,
) {
    let port = forward.guest_port();
    let mut accept_loops = JoinSet::new();
    let (guest, served) = (guest.clone(), state.clone());
    reverse_forward::serve_with(forward, &mut accept_loops, move |stream| {
        let (guest, forward) = (guest.clone(), served.clone());
        async move {
            if !forward.is_open() {
                debug!("sign-in callback on {port} is closed; dropped a connection");
                return Ok(());
            }
            let rpc_io = guest.connect(port).await?;
            relay_callback(stream, rpc_io, forward).await
        }
    });
    tasks.spawn_local(async move {
        state.closed().await;
        debug!("sign-in callback on {port} closed; freeing the port");
        // The accept loops, and their listeners, end with their set.
        drop(accept_loops);
    });
}

type CallbackBody = Either<Incoming, Full<Bytes>>;

/// Serve the browser's HTTP/1 requests on `browser` and send each to the
/// guest over `guest`, hardened as the module docs say.
async fn relay_callback<B, G>(browser: B, guest: G, forward: CallbackForward) -> anyhow::Result<()>
where
    B: AsyncRead + AsyncWrite + Unpin + 'static,
    G: AsyncRead + AsyncWrite + Unpin + 'static,
{
    let (sender, guest_conn) =
        hyper::client::conn::http1::handshake::<_, Incoming>(TokioIo::new(guest)).await?;
    let sender = Rc::new(RefCell::new(sender));
    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let (sender, forward) = (sender.clone(), forward.clone());
        async move {
            if !forward.is_open() {
                return Ok(finished_page());
            }
            if req.method() != Method::GET {
                warn!(
                    "refused: sign-in callback: a {} request (answered 405)",
                    req.method()
                );
                let mut resp = status_only(StatusCode::METHOD_NOT_ALLOWED);
                resp.headers_mut()
                    .insert(ALLOW, HeaderValue::from_static("GET"));
                return Ok(resp);
            }
            let origin = forward.origin(&req);
            let callback = forward.0.callback.get();
            let req = match to_guest(req, &forward) {
                Ok(req) => req,
                Err(e) => {
                    warn!("refused: sign-in callback: {e:#} (answered 400)");
                    return Ok(status_only(StatusCode::BAD_REQUEST));
                }
            };
            // The borrow ends before the send is awaited.
            let sent = sender.borrow_mut().send_request(req);
            match sent.await {
                Ok(resp) => Ok::<_, hyper::Error>(to_browser(resp, &origin, callback.pages)),
                Err(e) => {
                    warn!(
                        "sign-in callback: the agent in the sandbox did not answer: {e} \
                         (answered 502)"
                    );
                    Ok(status_only(StatusCode::BAD_GATEWAY))
                }
            }
        }
    });
    let browser =
        hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(browser), service);
    // The guest may close first (`Connection: close`): the browser still
    // gets the answer hyper has already received.
    let mut browser = std::pin::pin!(browser);
    tokio::select! {
        served = browser.as_mut() => return served.context("sign-in callback"),
        ended = guest_conn => {
            if let Err(e) = ended {
                debug!("sign-in callback to the guest: {e}");
            }
        }
    }
    browser.await.context("sign-in callback")
}

/// The browser's request as the guest gets it: no `Cookie` and no
/// `Authorization`, and every `code` of its query swapped for a surrogate
/// code of the forward's sign-in. The first request with a `code` starts
/// the [`FOLLOW_UP`] time.
fn to_guest(
    mut req: Request<Incoming>,
    forward: &CallbackForward,
) -> anyhow::Result<Request<Incoming>> {
    req.headers_mut().remove(COOKIE);
    req.headers_mut().remove(AUTHORIZATION);
    let Some(query) = req.uri().query() else {
        return Ok(req);
    };
    let state = &forward.0;
    let channel = Channel::Callback(state.port);
    let Some(query) = state
        .codes
        .rewrite_query(query, state.callback.get().service, channel)?
    else {
        return Ok(req);
    };
    if state.code_seen.get().is_none() {
        state.code_seen.set(Some(Instant::now()));
    }
    let path_and_query = format!("{}?{query}", req.uri().path());
    *req.uri_mut() = Uri::try_from(path_and_query).context("rewritten callback URI")?;
    Ok(req)
}

/// The guest's answer as the browser gets it: only the headers of
/// [`ANSWER_HEADERS`], the sandbox CSP, and a redirect only to `origin` or
/// an `https` page on one of `pages`; any other redirect becomes the
/// host's [`finished_page`].
fn to_browser(resp: Response<Incoming>, origin: &Url, pages: &[&str]) -> Response<CallbackBody> {
    let redirect = resp.status().is_redirection() && resp.headers().contains_key(LOCATION);
    if redirect {
        let allowed = resp
            .headers()
            .get(LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| origin.join(l).ok())
            .is_some_and(|to| redirect_allowed(&to, origin, pages));
        if !allowed {
            warn!(
                "refused: sign-in callback: a redirect of the agent to a page that is no \
                 loopback origin or sign-in page (showed the finished page)"
            );
            return finished_page();
        }
    }
    let (mut parts, body) = resp.into_parts();
    let mut headers = HeaderMap::new();
    let location = redirect.then_some(LOCATION);
    for name in ANSWER_HEADERS.into_iter().chain(location) {
        for value in parts.headers.get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    parts.headers = headers;
    Response::from_parts(parts, Either::Left(body))
}

/// A redirect target the guest may send the browser to.
fn redirect_allowed(to: &Url, origin: &Url, pages: &[&str]) -> bool {
    if !to.username().is_empty() || to.password().is_some() {
        return false;
    }
    match to.scheme() {
        "http" => {
            to.host_str() == origin.host_str()
                && to.port_or_known_default() == origin.port_or_known_default()
        }
        "https" => to.port().is_none() && to.host_str().is_some_and(|h| pages.contains(&h)),
        _ => false,
    }
}

/// The host's own answer: the sign-in is over.
fn finished_page() -> Response<CallbackBody> {
    let mut resp = Response::new(Either::Right(Full::new(Bytes::from_static(
        FINISHED_PAGE.as_bytes(),
    ))));
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn status_only(status: StatusCode) -> Response<CallbackBody> {
    let mut resp = Response::new(Either::Right(Full::new(Bytes::new())));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    resp
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::test_support::block_on_local;

    const PORT: u16 = 1455;
    const CALLBACK: Callback = Callback {
        service: ServiceId::Openai,
        pages: &["auth.openai.com", "chatgpt.com"],
    };

    /// A request as the guest's callback server got it.
    #[derive(Clone, Debug)]
    struct Got {
        target: String,
        headers: hyper::HeaderMap,
    }

    /// The guest's callback server: records each request, answers with
    /// `answer` (status and headers), or `200 ok`.
    async fn fake_guest<S: AsyncRead + AsyncWrite + Unpin + 'static>(
        io: S,
        seen: Arc<Mutex<Vec<Got>>>,
        answer: Vec<(&'static str, &'static str)>,
        status: u16,
    ) {
        let service = hyper::service::service_fn(move |req: Request<Incoming>| {
            seen.lock().unwrap().push(Got {
                target: req.uri().to_string(),
                headers: req.headers().clone(),
            });
            let mut resp = Response::new(Full::new(Bytes::from("ok")));
            *resp.status_mut() = StatusCode::from_u16(status).unwrap();
            for (k, v) in &answer {
                resp.headers_mut().append(*k, HeaderValue::from_static(v));
            }
            async { Ok::<_, hyper::Error>(resp) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(io), service)
            .await;
    }

    struct Exchanged {
        answer: String,
        seen: Vec<Got>,
        codes: PendingCodes,
    }

    impl Exchanged {
        fn targets(&self) -> Vec<String> {
            self.seen.iter().map(|g| g.target.clone()).collect()
        }
    }

    /// Send `raw` from the browser's side through `forward` to a guest
    /// that answers `status` with `answer` headers; read the whole answer.
    async fn exchange_with(
        forward: &CallbackForward,
        raw: &str,
        answer: Vec<(&'static str, &'static str)>,
        status: u16,
    ) -> Exchanged {
        let (mut browser, browser_end) = tokio::io::duplex(64 * 1024);
        let (guest, guest_end) = tokio::io::duplex(64 * 1024);
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::task::spawn_local(fake_guest(guest_end, seen.clone(), answer, status));
        let relay = tokio::task::spawn_local(relay_callback(browser_end, guest, forward.clone()));
        browser.write_all(raw.as_bytes()).await.unwrap();
        let mut answer = String::new();
        browser.read_to_string(&mut answer).await.unwrap();
        let _ = relay.await;
        let seen = seen.lock().unwrap().clone();
        Exchanged {
            answer,
            seen,
            codes: forward.0.codes.clone(),
        }
    }

    fn forward() -> CallbackForward {
        CallbackForward::new(PORT, CALLBACK, PendingCodes::default())
    }

    async fn exchange(raw: &str) -> Exchanged {
        exchange_with(&forward(), raw, vec![], 200).await
    }

    fn get(target: &str, extra: &str) -> String {
        format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n{extra}Connection: close\r\n\r\n"
        )
    }

    #[test]
    fn every_callback_request_gets_a_surrogate_code() {
        block_on_local(async {
            let got = exchange(&format!(
                "GET /callback?code=real-1&state=s HTTP/1.1\r\nHost: localhost\r\n\r\n{}",
                get("/callback?state=t&code=real-2", "")
            ))
            .await;
            assert_eq!(got.answer.matches("200 OK").count(), 2, "{}", got.answer);
            assert_eq!(got.seen.len(), 2, "{:?}", got.seen);
            let codes: Vec<String> = got
                .targets()
                .iter()
                .map(|target| {
                    assert!(!target.contains("real-"), "{target}");
                    let query = target.split_once('?').unwrap().1;
                    url::form_urlencoded::parse(query.as_bytes())
                        .find(|(k, _)| k == "code")
                        .unwrap()
                        .1
                        .into_owned()
                })
                .collect();
            // Bound to the forward's service and port.
            let other = Channel::Callback(PORT);
            assert_eq!(
                got.codes.redeem(&codes[0], ServiceId::Anthropic, other),
                None
            );
            assert_eq!(
                got.codes
                    .redeem(&codes[1], ServiceId::Openai, Channel::Callback(PORT))
                    .as_deref(),
                Some("real-2")
            );
        });
    }

    #[test]
    fn a_request_without_code_passes_unchanged() {
        block_on_local(async {
            let got = exchange(&get("/success?id_token=x&a=%2B", "")).await;
            assert_eq!(got.targets(), ["/success?id_token=x&a=%2B"]);
        });
    }

    #[test]
    fn a_malformed_request_never_reaches_the_guest() {
        block_on_local(async {
            let got = exchange("GARBAGE /callback?code=real HTTP/9\r\n\x00\r\n\r\n").await;
            assert!(got.seen.is_empty(), "{:?}", got.seen);
            assert!(!got.answer.contains("real"), "{}", got.answer);
        });
    }

    /// Only `GET` reaches the guest, without the browser's cookies and
    /// credentials.
    #[test]
    fn only_get_reaches_the_guest_without_cookies() {
        block_on_local(async {
            let got = exchange(
                "POST /callback?code=real HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n",
            )
            .await;
            assert!(got.seen.is_empty(), "{:?}", got.seen);
            assert!(got.answer.starts_with("HTTP/1.1 405"), "{}", got.answer);

            let got = exchange(&get(
                "/callback?code=real",
                "Cookie: session=secret\r\nAuthorization: Bearer secret\r\n",
            ))
            .await;
            let headers = &got.seen[0].headers;
            assert!(headers.get(COOKIE).is_none(), "{headers:?}");
            assert!(headers.get(AUTHORIZATION).is_none(), "{headers:?}");
        });
    }

    /// The guest's answer reaches the browser with only the allowed
    /// headers (no cookies, CORS or site-data commands) and the sandbox
    /// CSP.
    #[test]
    fn answers_keep_only_allowed_headers_and_get_the_sandbox_csp() {
        block_on_local(async {
            let got = exchange_with(
                &forward(),
                &get("/callback?code=real", ""),
                vec![
                    ("set-cookie", "session=evil; Domain=localhost"),
                    ("refresh", "0; url=https://evil.example/"),
                    ("content-security-policy", "default-src *"),
                    ("access-control-allow-origin", "*"),
                    ("clear-site-data", "\"*\""),
                    ("location", "https://evil.example/"),
                    ("x-custom", "1"),
                    ("content-type", "text/html"),
                    ("cache-control", "no-store"),
                ],
                200,
            )
            .await;
            let answer = got.answer.to_ascii_lowercase();
            assert!(answer.starts_with("http/1.1 200"), "{answer}");
            for refused in [
                "set-cookie",
                "refresh:",
                "access-control-allow-origin",
                "clear-site-data",
                "location",
                "x-custom",
            ] {
                assert!(!answer.contains(refused), "{refused}: {answer}");
            }
            assert!(answer.contains("content-type: text/html"), "{answer}");
            assert!(answer.contains("cache-control: no-store"), "{answer}");
            assert!(answer.ends_with("\r\n\r\nok"), "{answer}");
            assert!(
                answer.contains("content-security-policy: sandbox; default-src 'none'"),
                "{answer}"
            );
            assert!(!answer.contains("default-src *"), "{answer}");
        });
    }

    /// A redirect leads only to the same loopback origin or to a page of
    /// the service; others get the host's page.
    #[test]
    fn redirects_lead_only_to_known_places() {
        block_on_local(async {
            for (location, allowed) in [
                ("/success?id_token=x", true),
                ("http://127.0.0.1:1455/success", true),
                ("https://chatgpt.com/", true),
                ("https://auth.openai.com/log-in", true),
                ("http://localhost:1455/success", false),
                ("http://127.0.0.1:1456/success", false),
                ("https://evil.example/", false),
                ("https://chatgpt.com:8443/", false),
                ("https://u:p@chatgpt.com/", false),
                ("http://chatgpt.com/", false),
                ("https://platform.claude.com/oauth/code/success", false),
                ("javascript:alert(1)", false),
                ("//evil.example/x", false),
            ] {
                let got = exchange_with(
                    &forward(),
                    &get("/auth/callback?code=real", ""),
                    vec![("location", location)],
                    302,
                )
                .await;
                if allowed {
                    assert!(
                        got.answer.starts_with("HTTP/1.1 302"),
                        "{location}: {}",
                        got.answer
                    );
                    assert!(got.answer.contains(location), "{location}: {}", got.answer);
                } else {
                    assert!(
                        got.answer.starts_with("HTTP/1.1 200"),
                        "{location}: {}",
                        got.answer
                    );
                    assert!(
                        got.answer
                            .contains("Sign-in finished; return to the terminal."),
                        "{location}: {}",
                        got.answer
                    );
                    assert!(!got.answer.contains(location), "{location}: {}", got.answer);
                }
            }
        });
    }

    /// Once a code came, follow-ups reach the guest for [`FOLLOW_UP`] only;
    /// a reopened forward lets a new sign-in through.
    #[test]
    fn the_forward_closes_after_the_follow_up_time() {
        block_on_local(async {
            let forward = forward();
            let got = exchange_with(&forward, &get("/callback?code=real", ""), vec![], 200).await;
            assert_eq!(got.seen.len(), 1);
            let got = exchange_with(&forward, &get("/success", ""), vec![], 200).await;
            assert_eq!(got.seen.len(), 1, "a follow-up in time reaches the guest");

            let past = Instant::now().checked_sub(FOLLOW_UP).unwrap();
            forward.0.code_seen.set(Some(past));
            assert!(!forward.is_open());
            let got = exchange_with(&forward, &get("/success", ""), vec![], 200).await;
            assert!(got.seen.is_empty(), "{:?}", got.seen);
            assert!(got.answer.contains("Sign-in finished"), "{}", got.answer);

            assert!(forward.reopen(CALLBACK));
            assert!(forward.is_open());
            let got = exchange_with(&forward, &get("/callback?code=real", ""), vec![], 200).await;
            assert_eq!(got.seen.len(), 1);
        });
    }

    /// A forward that gets no code closes after [`UNUSED_LIMIT`]; a new
    /// sign-in on its port before then opens it again.
    #[test]
    fn an_unused_forward_closes() {
        let forward = forward();
        assert!(forward.is_open());
        let past = Instant::now().checked_sub(UNUSED_LIMIT).unwrap();
        forward.0.opened.set(past);
        assert!(!forward.is_open());
        assert!(forward.reopen(CALLBACK));
        assert!(forward.is_open());
    }

    /// A supervisor that implements nothing: no connection reaches it.
    struct NoSupervisor;
    impl airlock_common::supervisor_capnp::supervisor::Server for NoSupervisor {}

    /// A closed forward frees its port and cannot be reopened.
    #[test]
    fn a_closed_forward_frees_its_port() {
        block_on_local(async {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let bound = reverse_forward::bind_exclusive(port, port).unwrap();
            let guest = GuestNetwork::new(capnp_rpc::new_client(NoSupervisor));
            let state = CallbackForward::new(port, CALLBACK, PendingCodes::default());
            let past = Instant::now().checked_sub(UNUSED_LIMIT).unwrap();
            state.0.opened.set(past);
            let mut tasks = JoinSet::new();
            serve_until_closed(bound, &guest, &mut tasks, state.clone());
            let mut freed = false;
            for _ in 0..50 {
                tokio::task::yield_now().await;
                if let Ok(l) = reverse_forward::bind_exclusive(port, port) {
                    drop(l);
                    freed = true;
                    break;
                }
            }
            assert!(freed, "the closed forward still holds port {port}");
            assert!(!state.reopen(CALLBACK));
            assert!(!state.is_open());
        });
    }
}
