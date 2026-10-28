//! Legacy SSE transport.
//!
//! Opens one event stream to the server. The first event of interest
//! gives the URL for the guest messages. That URL must have the same
//! origin and the same user information as the server URL, so that the
//! headers do not go to another server. After that, the stream carries
//! all server messages, and the guest messages go to that URL one after
//! the other. The session stops when the stream ends. There is no
//! reconnect.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc;
use url::Url;

use super::{
    EventStream, MAX_MESSAGE, Relay, RemoteSpec, StreamError, client, has_media_type,
    is_message_event, origin, parse_message, request_ids,
};
use crate::messages;

/// Time for the server to give the message URL.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(30);

/// Open event stream of a server, with its message URL.
pub(in crate::mcp) struct Stream {
    /// Events of the stream after the endpoint event.
    events: EventStream,
    /// URL for the guest messages.
    endpoint: Url,
    /// Headers of the owner.
    headers: HeaderMap,
}

/// Open the event stream of `remote` and wait for the message URL.
/// Returns:
///   The open stream, or the problem for the log.
pub(super) async fn connect(name: &str, remote: &RemoteSpec) -> Result<Stream, String> {
    let at = origin(&remote.url);
    let opened = tokio::time::timeout(ENDPOINT_TIMEOUT, async {
        let mut headers = remote.header_map();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let response = client()
            .get(remote.url.clone())
            .headers(headers.clone())
            .send()
            .await
            .map_err(|e| format!("GET {at}: {}", e.without_url()))?;
        if !response.status().is_success() {
            return Err(format!("GET {at}: HTTP {}", response.status()));
        }
        if !has_media_type(&response, "text/event-stream") {
            return Err(format!("GET {at}: not an event stream"));
        }
        let mut events = EventStream::new(response);
        loop {
            let event = match events.next().await {
                Ok(Some(event)) => event,
                Ok(None) => return Err(format!("GET {at}: stream ended before the endpoint")),
                Err(StreamError::TooLong) => {
                    return Err(format!("GET {at}: event longer than {MAX_MESSAGE} bytes"));
                }
                Err(StreamError::Read(e)) => return Err(format!("GET {at}: {e}")),
            };
            if event.kind != "endpoint" {
                continue;
            }
            let endpoint = remote
                .url
                .join(event.data.trim())
                .map_err(|e| format!("GET {at}: bad endpoint: {e}"))?;
            // The headers of the owner go to the endpoint too. Thus it must
            // be the same server. A relative endpoint gets the user
            // information of the server URL from the join.
            if endpoint.origin() != remote.url.origin()
                || endpoint.username() != remote.url.username()
                || endpoint.password() != remote.url.password()
            {
                return Err(format!(
                    "GET {at}: endpoint on other origin {}",
                    origin(&endpoint)
                ));
            }
            return Ok(Stream {
                events,
                endpoint,
                headers: remote.header_map(),
            });
        }
    })
    .await
    .unwrap_or_else(|_| Err(format!("GET {at}: no endpoint in time")));
    tracing::debug!(
        "mcp {name}: legacy SSE {at}: {}",
        if opened.is_ok() { "open" } else { "failed" }
    );
    opened
}

/// Start the tasks of one session.
/// Args:
///  - `stream`: The open stream from [`connect`]
///  - `relay`: Path to the guest
///  - `messages`: Queue of the guest messages
pub(super) fn start(stream: Stream, relay: &Relay, messages: mpsc::Receiver<String>) {
    let Stream {
        events,
        endpoint,
        headers,
    } = stream;
    relay.spawn(read_stream(events, relay.clone()));
    relay.spawn(send_messages(endpoint, headers, relay.clone(), messages));
}

/// Send each server message of the stream to the guest. Stop the session
/// at the end of the stream.
async fn read_stream(mut events: EventStream, relay: Relay) {
    let reason = loop {
        match events.next().await {
            Ok(Some(event)) => {
                if !is_message_event(&event.kind) {
                    continue;
                }
                if let Some(message) = parse_message(event.data.as_bytes()) {
                    relay.deliver(&message).await;
                } else {
                    tracing::debug!("mcp {}: server message is not JSON-RPC", relay.name);
                }
            }
            Ok(None) => break messages::mcp::REMOTE_STREAM_ENDED.to_string(),
            Err(StreamError::TooLong) => break messages::mcp::remote_too_long(MAX_MESSAGE),
            Err(StreamError::Read(e)) => {
                tracing::debug!("mcp {}: legacy SSE stream: {e}", relay.name);
                break messages::mcp::REMOTE_STREAM_ENDED.to_string();
            }
        }
    };
    relay.stop(reason);
}

/// Send each guest message to the message URL, in order. Give the guest
/// an error for each request of a failed message.
async fn send_messages(
    endpoint: Url,
    headers: HeaderMap,
    relay: Relay,
    mut messages: mpsc::Receiver<String>,
) {
    let at = origin(&endpoint);
    while let Some(message) = messages.recv().await {
        let parsed: Option<Value> = serde_json::from_str(&message).ok();
        let ids = request_ids(parsed.as_ref());
        let mut headers = headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let sent = client()
            .post(endpoint.clone())
            .headers(headers)
            .body(message)
            .send()
            .await;
        let problem = match sent {
            Ok(response) if response.status().is_success() => continue,
            Ok(response) => format!("HTTP {}", response.status()),
            Err(e) => e.without_url().to_string(),
        };
        tracing::debug!("mcp {} POST {at}: {problem}", relay.name);
        relay.fail(ids).await;
    }
}
