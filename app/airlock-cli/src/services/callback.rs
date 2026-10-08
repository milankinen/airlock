//! The sign-in callback forward.
//!
//! A sign-in tool in the guest waits for the OAuth callback on a loopback
//! port. The callback forward receives the browser's redirect to that port
//! on the host and sends it to the tool in the guest.
//!
//! The forward is not a raw relay. It replaces the authorization code with
//! a surrogate code, so the real code never gets to the guest. It also
//! protects the browser, which holds the user's sessions, from the
//! untrusted guest.

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

/// How long the browser's follow-up requests (for example, a success page)
/// get to the guest after the first request with a `code`.
const FOLLOW_UP: Duration = Duration::from_mins(1);

/// How long a forward waits for its first request with a `code`. Thus the
/// sandbox cannot hold a host port forever.
const UNUSED_LIMIT: Duration = Duration::from_mins(10);

/// Headers of the guest's answers that get to the browser. A `Location`
/// also gets to it, on a redirect that [`to_browser`] allows. Thus no
/// `Set-Cookie`, `Refresh`, CORS or `Clear-Site-Data` header gets to the
/// browser.
const ANSWER_HEADERS: [HeaderName; 3] = [CONTENT_TYPE, CONTENT_LENGTH, CACHE_CONTROL];

/// Content security policy of the callback answers to the browser.
const CSP: &str = "sandbox; default-src 'none'";

/// Page that the host shows in place of a guest answer that it refuses.
const FINISHED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>airlock</title>\
    <p>Sign-in finished; return to the terminal.</p>\n";

/// The sign-in that a callback forward serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Callback {
    /// The service whose sign-in page named the callback.
    pub service: ServiceId,
    /// Hosts of the service's pages that a redirect can go to (`https`).
    pub pages: &'static [&'static str],
}

/// The state of one callback forward, shared by its listeners and
/// connections. Cheap to clone.
///
/// A forward closes [`FOLLOW_UP`] after the first request with a `code`,
/// or [`UNUSED_LIMIT`] after it opened if no `code` came. A closed forward
/// drops new connections, answers new requests with the host page, and
/// frees its port.
#[derive(Clone)]
pub struct CallbackForward(Rc<ForwardState>);

/// Runtime state of a forward. A new sign-in on the same port opens it
/// again with its own callback. The first code starts the follow-up time.
struct ForwardState {
    port: u16,
    codes: PendingCodes,
    callback: Cell<Callback>,
    /// When the forward opened, or when a new sign-in opened it again.
    opened: Cell<Instant>,
    /// When the first request with a `code` came.
    code_seen: Cell<Option<Instant>>,
    /// True if the forward closed permanently. Its port is free, or
    /// becomes free.
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

    /// Open the forward again for a new sign-in of `callback`.
    /// Returns:
    ///   `false` if the forward closed permanently. Then the sign-in needs
    ///   a new forward.
    pub fn reopen(&self, callback: Callback) -> bool {
        if self.0.ended.get() {
            return false;
        }
        self.0.callback.set(callback);
        self.0.opened.set(Instant::now());
        self.0.code_seen.set(None);
        true
    }

    /// Get the time when the forward closes: [`FOLLOW_UP`] after the first
    /// code, else [`UNUSED_LIMIT`] after it opened.
    fn deadline(&self) -> Instant {
        match self.0.code_seen.get() {
            Some(seen) => seen + FOLLOW_UP,
            None => self.0.opened.get() + UNUSED_LIMIT,
        }
    }

    /// Whether requests still get to the guest.
    fn is_open(&self) -> bool {
        !self.0.ended.get() && Instant::now() < self.deadline()
    }

    /// Wait until the forward closes, and mark it as closed permanently.
    async fn closed(&self) {
        while self.is_open() {
            tokio::time::sleep_until(self.deadline().into()).await;
        }
        self.0.ended.set(true);
    }

    /// Get the origin that the browser used for the forward, from `Host`.
    /// The origin is a loopback name with the forward's port. If `Host`
    /// is not valid, the origin is `127.0.0.1` with the port.
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

/// Forward the bound callback port into the guest for a sign-in.
///
/// Relays each connection to the same port on `guest`, and replaces the
/// `code` of each request with a surrogate code. A task in `tasks` runs
/// the accept loops until the forward closes, and then frees the port.
/// Args:
///  - `forward`: The bound callback port
///  - `guest`: The guest network
///  - `tasks`: Task set that gets the forward task
///  - `codes`: Store for the surrogate codes
///  - `callback`: The sign-in that the forward serves.
///
/// Returns:
///   The state of the forward.
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

/// Serve `forward` for `state` in a task of `tasks` until `state` closes.
/// Then free the port.
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

/// Serve the browser's HTTP/1 requests on `browser`, and send each request
/// to the guest over `guest`.
///
/// Only `GET` requests get to the guest. Other methods get `405` from the
/// host. A request that does not parse as HTTP/1 gets an error from the
/// host and never gets to the guest. See [`to_guest`] and [`to_browser`]
/// for the changes to requests and answers.
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
            // The borrow ends before the code awaits the send.
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
    // The guest can close first (`Connection: close`). The browser still
    // gets the answer that hyper already received.
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

/// Change the browser's request for the guest.
///
/// Removes `Cookie` and `Authorization`. Replaces every `code` of the
/// query with a surrogate code, bound to the service and callback port of
/// the forward's sign-in. The first request with a `code` starts the
/// [`FOLLOW_UP`] time.
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

/// Change the guest's answer for the browser.
///
/// Keeps only the headers of [`ANSWER_HEADERS`] and adds the sandbox
/// [`CSP`]. A redirect (`3xx` with `Location`) can go only to the same
/// loopback origin (host and port) or to an `https` page on one of
/// `pages`. Any other redirect becomes the host's [`finished_page`].
/// Args:
///  - `resp`: The guest's answer
///  - `origin`: The loopback origin that the browser used
///  - `pages`: Hosts of the service's pages.
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

/// Whether the guest can send the browser to the redirect target `to`.
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

/// Make the host's own answer: the sign-in is finished.
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

/// Make an empty answer with `status` and the sandbox CSP.
fn status_only(status: StatusCode) -> Response<CallbackBody> {
    let mut resp = Response::new(Either::Right(Full::new(Bytes::new())));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    resp
}

#[cfg(test)]
mod tests {
    //! The sign-in callback forward: code replacement, what gets to the
    //! guest and to the browser, and when the forward closes.

    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::test_cfg::block_on_local;
    use crate::test_cfg::services::idle_guest;

    const PORT: u16 = 1455;
    const CALLBACK: Callback = Callback {
        service: ServiceId::Openai,
        pages: &["auth.openai.com", "chatgpt.com"],
    };

    /// One request that the fake guest received.
    #[derive(Clone, Debug)]
    struct Got {
        target: String,
        headers: hyper::HeaderMap,
    }

    /// Serve HTTP/1 on `io` like the sign-in tool in the guest. Record each
    /// request in `seen` and answer with `status`, the headers in `answer`
    /// and the body "ok".
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

    /// The result of one browser exchange through the forward.
    struct Exchanged {
        answer: String,
        seen: Vec<Got>,
        codes: PendingCodes,
    }

    impl Exchanged {
        /// The request targets that the guest received.
        fn targets(&self) -> Vec<String> {
            self.seen.iter().map(|g| g.target.clone()).collect()
        }
    }

    /// Send the raw browser bytes `raw` through `forward` to a fake guest
    /// that answers with `answer` headers and `status`. Return the browser's
    /// answer, the guest's requests and the pending codes.
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

    /// A new forward for the OpenAI callback on the test port.
    fn forward() -> CallbackForward {
        CallbackForward::new(PORT, CALLBACK, PendingCodes::default())
    }

    /// Send `raw` through a new forward to a guest that answers 200.
    async fn exchange(raw: &str) -> Exchanged {
        exchange_with(&forward(), raw, vec![], 200).await
    }

    /// A browser GET of `target` with the `extra` header lines that closes
    /// the connection.
    fn get(target: &str, extra: &str) -> String {
        format!(
            "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n{extra}Connection: close\r\n\r\n"
        )
    }

    /// Test that each callback request on a connection gets a surrogate code
    /// that redeems only for its service and port. The real code must never
    /// get to the guest.
    ///   1. Send two callback requests with real codes on one connection
    ///   2. Check that the guest gets both, with no real code and with the
    ///      other query values unchanged
    ///   3. Check that a surrogate does not redeem for a different service
    ///   4. Check that a surrogate redeems to its real code for its service
    #[test]
    fn every_callback_request_gets_surrogate_code_bound_to_service_and_port() {
        block_on_local(async {
            // The first request keeps the connection open, so both requests
            // use one connection.
            let got = exchange(&format!(
                "GET /callback?code=real-1&state=a%20b HTTP/1.1\r\nHost: localhost\r\n\r\n{}",
                get("/callback?state=t&code=real-2", "")
            ))
            .await;
            assert_eq!(got.answer.matches("200 OK").count(), 2, "{}", got.answer);
            assert_eq!(got.seen.len(), 2, "{:?}", got.seen);
            let queries: Vec<std::collections::HashMap<String, String>> = got
                .targets()
                .iter()
                .map(|target| {
                    assert!(!target.contains("real-"), "{target}");
                    let query = target.split_once('?').unwrap().1;
                    url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect()
                })
                .collect();
            assert_eq!(queries[0]["state"], "a b");
            assert_eq!(queries[1]["state"], "t");
            assert_eq!(
                got.codes.redeem(
                    &queries[0]["code"],
                    ServiceId::Anthropic,
                    Channel::Callback(PORT)
                ),
                None
            );
            assert_eq!(
                got.codes
                    .redeem(
                        &queries[1]["code"],
                        ServiceId::Openai,
                        Channel::Callback(PORT)
                    )
                    .as_deref(),
                Some("real-2")
            );
        });
    }

    /// Test that a request with no code gets to the guest unchanged.
    ///   1. Send a request to the success page with no code
    ///   2. Check that the guest gets the same target
    #[test]
    fn request_without_code_passes_unchanged() {
        block_on_local(async {
            let got = exchange(&get("/success?id_token=x&a=%2B", "")).await;
            assert_eq!(got.targets(), ["/success?id_token=x&a=%2B"]);
        });
    }

    /// Test that a malformed request does not get to the guest and that the
    /// answer does not show the code.
    ///   1. Send bytes that are not a valid HTTP request
    ///   2. Check that the guest gets nothing and the answer has no code
    #[test]
    fn malformed_request_never_reaches_guest() {
        block_on_local(async {
            let got = exchange("GARBAGE /callback?code=real HTTP/9\r\n\x00\r\n\r\n").await;
            assert!(got.seen.is_empty(), "{:?}", got.seen);
            assert!(!got.answer.contains("real"), "{}", got.answer);
        });
    }

    /// Test that only GET requests get to the guest, with no cookies or
    /// credentials. The browser must not leak user sessions to the guest.
    ///   1. Send a POST and check that the guest gets nothing and the browser
    ///      gets HTTP 405
    ///   2. Send a GET with a cookie and an authorization header
    ///   3. Check that the guest gets neither header
    #[test]
    fn only_get_reaches_guest_and_without_cookies_or_credentials() {
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

    /// Test that the guest answer keeps only the allowed headers and gets the
    /// sandbox content security policy. The guest must not set cookies, cause
    /// redirects or change browser state.
    ///   1. Let the guest answer with dangerous and safe headers
    ///   2. Check that the browser gets only the safe headers and the body
    ///   3. Check that the guest policy is replaced with the sandbox policy
    #[test]
    fn guest_answer_keeps_only_allowed_headers_and_gets_sandbox_csp() {
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
                "default-src *",
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
        });
    }

    /// Test that a guest redirect gets to the browser only if it goes to the
    /// same loopback origin or to an HTTPS page of the service. Otherwise the
    /// browser gets the host page. The guest must not send the browser to an
    /// attacker page.
    ///   1. Let the guest answer with a redirect to each location
    ///   2. Check that an allowed location gets to the browser as a redirect
    ///   3. Check that a refused location gives the host page with no
    ///      location
    #[test]
    fn guest_redirect_leads_only_to_same_loopback_origin_or_service_page() {
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
                let answer = &got.answer;
                if allowed {
                    assert!(answer.starts_with("HTTP/1.1 302"), "{location}: {answer}");
                    assert!(answer.contains(location), "{location}: {answer}");
                } else {
                    assert!(answer.starts_with("HTTP/1.1 200"), "{location}: {answer}");
                    assert!(
                        answer.contains("Sign-in finished; return to the terminal."),
                        "{location}: {answer}"
                    );
                    assert!(!answer.contains(location), "{location}: {answer}");
                }
            }
        });
    }

    /// Test that the forward closes when the follow-up time after the first
    /// code ends, and that a new sign-in opens it again.
    ///   1. Send a callback with a code and a follow-up request and check that
    ///      both get to the guest
    ///   2. Move the time of the code back by the follow-up time
    ///   3. Check that the forward is closed and answers with the host page
    ///   4. Open it again and check that a callback gets to the guest
    #[test]
    fn forward_closes_after_follow_up_time_until_reopened() {
        block_on_local(async {
            let forward = forward();
            let got = exchange_with(&forward, &get("/callback?code=real", ""), vec![], 200).await;
            assert_eq!(got.seen.len(), 1);
            let got = exchange_with(&forward, &get("/success", ""), vec![], 200).await;
            assert_eq!(got.seen.len(), 1);

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

    /// Test that a forward that gets no code closes after the unused limit,
    /// and that a new sign-in opens it again. The sandbox must not hold a host
    /// port forever.
    ///   1. Move the open time of the forward back by the unused limit
    ///   2. Check that the forward is closed
    ///   3. Open it again and check that it is open
    #[test]
    fn forward_without_code_closes_after_unused_limit_until_reopened() {
        let forward = forward();
        assert!(forward.is_open());
        let past = Instant::now().checked_sub(UNUSED_LIMIT).unwrap();
        forward.0.opened.set(past);
        assert!(!forward.is_open());
        assert!(forward.reopen(CALLBACK));
        assert!(forward.is_open());
    }

    /// Test that a closed forward frees its host port and cannot open again.
    ///   1. Bind a free port and serve a forward that is past its unused limit
    ///   2. Wait until the port can be bound again
    ///   3. Check that the forward cannot open again
    #[test]
    fn closed_forward_frees_its_port_and_cannot_reopen() {
        block_on_local(async {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let bound = reverse_forward::bind_exclusive(port, port).unwrap();
            let state = CallbackForward::new(port, CALLBACK, PendingCodes::default());
            let past = Instant::now().checked_sub(UNUSED_LIMIT).unwrap();
            state.0.opened.set(past);
            let mut tasks = JoinSet::new();
            serve_until_closed(bound, &idle_guest(), &mut tasks, state.clone());
            // The serve task runs on this thread. Yield to let it close the
            // forward and drop the listener.
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
