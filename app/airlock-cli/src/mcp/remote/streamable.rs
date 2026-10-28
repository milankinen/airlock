//! Streamable HTTP transport.
//!
//! Sends each guest message as one request. The first request, the
//! initialization, opens the session: the messages after it wait until
//! its answer starts, because the answer can give the session ID. After
//! that, requests run at the same time, with a limit. The server answers
//! with one JSON message or with an event stream of messages. After the
//! initialization, one separate event stream gets the messages that the
//! server starts. It reconnects a few times when it breaks.
//!
//! When an event stream answer breaks before the answer of its request,
//! the guest gets an error for the request. The relay does not resume
//! that stream.
//!
//! When the server says that the session does not exist any more, the
//! session stops. At the stop, the relay asks the server to end the
//! session, if the server gave a session ID.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use url::Url;

use super::{
    EventStream, MAX_MESSAGE, Relay, RemoteSpec, StreamError, client, has_media_type,
    is_message_event, mark_answered, origin, parse_message, read_body, request_ids, spawn_delete,
};
use crate::messages;

/// Session ID header.
const SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");

/// Protocol version header.
const PROTOCOL_VERSION: HeaderName = HeaderName::from_static("mcp-protocol-version");

/// Last event ID header, for a reconnect of the event stream.
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

/// Largest number of requests at the same time.
const MAX_POSTS: usize = 64;

/// Number of reconnects of the server event stream without an event in
/// between.
const MAX_RECONNECTS: u32 = 3;

/// Largest reconnect time that the server can ask for.
const MAX_RETRY: Duration = Duration::from_secs(30);

/// Time limit of the request that ends the session on the server.
const DELETE_TIMEOUT: Duration = Duration::from_secs(5);

/// State of one session that its tasks share.
struct Session {
    /// URL of the server.
    url: Url,
    /// Headers of the owner.
    headers: HeaderMap,
    /// Path to the guest.
    relay: Relay,
    /// Session ID from the server.
    id: RefCell<Option<HeaderValue>>,
    /// Protocol version from the initialization result.
    version: RefCell<Option<HeaderValue>>,
    /// Set when the server said that the session does not exist.
    expired: Cell<bool>,
    /// Limit of the requests at the same time.
    posts: Arc<Semaphore>,
}

/// Start the tasks of one session.
/// Args:
///  - `remote`: Address of the server
///  - `relay`: Path to the guest
///  - `messages`: Queue of the guest messages
///
/// Returns:
///   Function to call one time when the session stopped. It ends the
///   session on the server.
pub(super) fn start(
    remote: &RemoteSpec,
    relay: Relay,
    messages: mpsc::Receiver<String>,
) -> Box<dyn FnOnce()> {
    let session = Rc::new(Session {
        url: remote.url.clone(),
        headers: remote.header_map(),
        relay,
        id: RefCell::default(),
        version: RefCell::default(),
        expired: Cell::new(false),
        posts: Arc::new(Semaphore::new(MAX_POSTS)),
    });
    session.relay.spawn(dispatch(session.clone(), messages));
    Box::new(move || session.delete())
}

/// Send each guest message to the server.
async fn dispatch(session: Rc<Session>, mut messages: mpsc::Receiver<String>) {
    let mut stream_started = false;
    while let Some(message) = messages.recv().await {
        let parsed: Option<Value> = serde_json::from_str(&message).ok();
        let ids = request_ids(parsed.as_ref());
        let method = parsed
            .as_ref()
            .and_then(|m| m.get("method"))
            .and_then(Value::as_str);
        if method == Some("initialize") {
            // The init gate: the next messages need the session ID from
            // this answer. Thus wait for its headers here.
            let Some(response) = session.post(message, &ids).await else {
                continue;
            };
            if !session.take_session_id(&response) {
                return;
            }
            session
                .relay
                .spawn(session.clone().relay_response(response, ids, true));
            continue;
        }
        let Ok(permit) = session.posts.clone().acquire_owned().await else {
            return;
        };
        // The server can start its own messages after the client finished
        // the initialization. Open its stream after the server accepted the
        // notification: a strict server refuses the stream before that.
        let open_stream = method == Some("notifications/initialized")
            && !stream_started
            && session.id.borrow().is_some();
        stream_started |= open_stream;
        let task = session.clone();
        session.relay.spawn(async move {
            let _permit = permit;
            if let Some(response) = task.post(message, &ids).await {
                if open_stream {
                    task.relay.spawn(task.clone().server_stream());
                }
                task.relay_response(response, ids, false).await;
            }
        });
    }
}

impl Session {
    /// Add the session ID and the protocol version, if known, to
    /// `headers`.
    /// Returns:
    ///   True if a session ID was added.
    fn add_session_headers(&self, headers: &mut HeaderMap) -> bool {
        if let Some(version) = &*self.version.borrow() {
            headers.insert(PROTOCOL_VERSION, version.clone());
        }
        match &*self.id.borrow() {
            Some(id) => {
                headers.insert(SESSION_ID, id.clone());
                true
            }
            None => false,
        }
    }

    /// Send one guest message. For a failed request, give the guest an
    /// error for each request ID in `ids`.
    /// Returns:
    ///   The success response, or `None` if the request failed.
    async fn post(&self, message: String, ids: &[Value]) -> Option<Response> {
        let mut headers = self.headers.clone();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let with_session = self.add_session_headers(&mut headers);
        let sent = client()
            .post(self.url.clone())
            .headers(headers)
            .body(message)
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!(
                    "mcp {} POST {}: {}",
                    self.relay.name,
                    origin(&self.url),
                    e.without_url()
                );
                self.relay.fail(ids.iter().cloned()).await;
                return None;
            }
        };
        let status = response.status();
        if status == StatusCode::NOT_FOUND && with_session {
            self.expire();
            return None;
        }
        if !status.is_success() {
            tracing::debug!(
                "mcp {} POST {}: HTTP {status}",
                self.relay.name,
                origin(&self.url)
            );
            self.relay.fail(ids.iter().cloned()).await;
            return None;
        }
        Some(response)
    }

    /// Keep the session ID of the initialization response `response`.
    /// Returns:
    ///   False if the ID is not valid. Then the session stops.
    fn take_session_id(&self, response: &Response) -> bool {
        let Some(id) = response.headers().get(SESSION_ID) else {
            return true;
        };
        // The spec allows only visible ASCII characters.
        let bytes = id.as_bytes();
        if bytes.is_empty() || !bytes.iter().all(|b| (0x21..=0x7e).contains(b)) {
            self.relay
                .stop(messages::mcp::REMOTE_BAD_SESSION_ID.to_string());
            return false;
        }
        *self.id.borrow_mut() = Some(id.clone());
        true
    }

    /// Send the messages of the success response `response` to the guest.
    /// Give the guest an error for each request in `ids` that did not get
    /// an answer.
    /// Args:
    ///  - `response`: Response to the guest message
    ///  - `ids`: Request IDs of the guest message
    ///  - `init`: Set for the initialization request. Its result gives the
    ///    protocol version.
    async fn relay_response(self: Rc<Self>, mut response: Response, ids: Vec<Value>, init: bool) {
        let mut pending = ids;
        if response.status() == StatusCode::ACCEPTED {
            // Accepted messages have no answer, also if the server sent a
            // body.
        } else if has_media_type(&response, "text/event-stream") {
            let mut events = EventStream::new(response);
            loop {
                match events.next().await {
                    Ok(Some(event)) => {
                        if is_message_event(&event.kind) {
                            self.deliver(event.data.as_bytes(), &mut pending, init)
                                .await;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        self.stream_failed(e, "POST");
                        break;
                    }
                }
            }
        } else if has_media_type(&response, "application/json") {
            match read_body(&mut response).await {
                Ok(body) => self.deliver(&body, &mut pending, init).await,
                Err(e) => {
                    tracing::debug!("mcp {} POST {}: {e}", self.relay.name, origin(&self.url));
                }
            }
        }
        if !pending.is_empty() {
            tracing::debug!(
                "mcp {} POST {}: no answer for {} requests",
                self.relay.name,
                origin(&self.url),
                pending.len()
            );
            self.relay.fail(pending).await;
        }
    }

    /// Send one server message to the guest. Remove its answered request
    /// IDs from `pending`. With `init`, keep the protocol version from the
    /// initialization result.
    async fn deliver(&self, data: &[u8], pending: &mut Vec<Value>, init: bool) {
        let Some(message) = parse_message(data) else {
            tracing::debug!("mcp {}: server message is not JSON-RPC", self.relay.name);
            return;
        };
        if init
            && let Some(version) = message
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
                .and_then(|v| HeaderValue::from_str(v).ok())
        {
            // Set it before the guest sees the result. Thus the next
            // guest message has it.
            *self.version.borrow_mut() = Some(version);
        }
        mark_answered(pending, &message);
        self.relay.deliver(&message).await;
    }

    /// Log the problem `e` of an event stream. A message over the size
    /// limit stops the session.
    fn stream_failed(&self, e: StreamError, method: &str) {
        match e {
            StreamError::TooLong => self.relay.stop(messages::mcp::remote_too_long(MAX_MESSAGE)),
            StreamError::Read(e) => tracing::debug!(
                "mcp {} {method} {}: {e}",
                self.relay.name,
                origin(&self.url)
            ),
        }
    }

    /// Get the messages that the server starts, from one event stream.
    /// Reconnect when the stream breaks, at most [`MAX_RECONNECTS`] times
    /// without an event in between.
    async fn server_stream(self: Rc<Self>) {
        let mut last_id: Option<String> = None;
        let mut retry: Option<Duration> = None;
        let mut reconnects = 0;
        loop {
            let mut headers = self.headers.clone();
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            if let Some(id) = last_id
                .as_deref()
                .and_then(|id| HeaderValue::from_str(id).ok())
            {
                headers.insert(LAST_EVENT_ID, id);
            }
            let with_session = self.add_session_headers(&mut headers);
            let sent = client().get(self.url.clone()).headers(headers).send().await;
            match sent {
                Ok(response) if response.status() == StatusCode::METHOD_NOT_ALLOWED => {
                    // The server has no such stream.
                    return;
                }
                Ok(response) if response.status() == StatusCode::NOT_FOUND && with_session => {
                    self.expire();
                    return;
                }
                Ok(response)
                    if response.status().is_success()
                        && has_media_type(&response, "text/event-stream") =>
                {
                    let mut events = EventStream::new(response);
                    // The last ID stays for the next reconnect until the
                    // server sends a new one.
                    events.parser.set_last_id(last_id.take());
                    loop {
                        match events.next().await {
                            Ok(Some(event)) => {
                                reconnects = 0;
                                if is_message_event(&event.kind) {
                                    self.deliver(event.data.as_bytes(), &mut Vec::new(), false)
                                        .await;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let fatal = matches!(e, StreamError::TooLong);
                                self.stream_failed(e, "GET");
                                if fatal {
                                    return;
                                }
                                break;
                            }
                        }
                    }
                    last_id = events.parser.last_id().map(str::to_string);
                    retry = events.parser.retry().or(retry);
                }
                Ok(response) => tracing::debug!(
                    "mcp {} GET {}: HTTP {}",
                    self.relay.name,
                    origin(&self.url),
                    response.status()
                ),
                Err(e) => tracing::debug!(
                    "mcp {} GET {}: {}",
                    self.relay.name,
                    origin(&self.url),
                    e.without_url()
                ),
            }
            if reconnects >= MAX_RECONNECTS {
                tracing::debug!(
                    "mcp {} GET {}: no more reconnects",
                    self.relay.name,
                    origin(&self.url)
                );
                return;
            }
            let delay = retry.map_or(Duration::from_secs(1 << reconnects), |r| r.min(MAX_RETRY));
            reconnects += 1;
            tokio::time::sleep(delay).await;
        }
    }

    /// Stop the session because the server does not know it any more.
    fn expire(&self) {
        self.expired.set(true);
        self.relay
            .stop(messages::mcp::REMOTE_SESSION_EXPIRED.to_string());
    }

    /// Ask the server to end the session, in the background. Only for a
    /// session with an ID that the server still knows.
    fn delete(&self) {
        if self.expired.get() {
            return;
        }
        let mut headers = self.headers.clone();
        if !self.add_session_headers(&mut headers) {
            return;
        }
        let request = client()
            .delete(self.url.clone())
            .headers(headers)
            .timeout(DELETE_TIMEOUT)
            .send();
        let name = self.relay.name.clone();
        let origin = origin(&self.url);
        spawn_delete(async move {
            match request.await {
                Ok(response) => {
                    tracing::debug!("mcp {name} DELETE {origin}: HTTP {}", response.status());
                }
                Err(e) => tracing::debug!("mcp {name} DELETE {origin}: {}", e.without_url()),
            }
        });
    }
}
