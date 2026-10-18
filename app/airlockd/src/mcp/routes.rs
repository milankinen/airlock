//! HTTP endpoints of the MCP Streamable HTTP transport.
//!
//! Checks the transport rules of the spec and gives the JSON-RPC messages
//! to the bridge. Event streams send a keepalive comment at intervals. A
//! client cannot resume a broken stream.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{ACCEPT, ORIGIN};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::Value;
use tracing::warn;

use super::bridge::{Bridge, BridgeError, MessageStream};
use super::host_name;

/// HTTP header that carries the session ID.
const SESSION_HEADER: &str = "mcp-session-id";

/// Time between keepalive comments on an idle event stream.
const KEEPALIVE: Duration = Duration::from_secs(15);

/// Origin hosts that can send requests. Other web pages cannot.
const ALLOWED_ORIGINS: [&str; 3] = ["mcp.airlock", "localhost", "127.0.0.1"];

/// State that all MCP handlers share.
struct McpState {
    bridge: Bridge,
    names: Vec<String>,
}

type AppState = Arc<McpState>;

/// Make the MCP routes. The routes do not check the `Host` header. The
/// local server sends only `mcp.airlock` requests here.
/// Args:
///  - `bridge`: Handle to the bridge task
///  - `names`: Names of the servers that the host grants
pub fn router(bridge: Bridge, names: Vec<String>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/{name}", get(listen).post(post).delete(delete))
        .layer(middleware::from_fn(check_origin))
        .with_state(Arc::new(McpState { bridge, names }))
}

/// List the server names, one on each line.
async fn index(State(st): State<AppState>) -> String {
    st.names.iter().flat_map(|n| [n.as_str(), "\n"]).collect()
}

/// Send one JSON-RPC message from the client to the server. A request gets
/// an event stream, other messages get HTTP 202.
async fn post(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !st.names.contains(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !accepts(&headers, &["application/json", "text/event-stream"]) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must list application/json and text/event-stream",
        )
            .into_response();
    }
    let message: Value = match serde_json::from_slice(&body) {
        Ok(Value::Object(obj)) => Value::Object(obj),
        Ok(Value::Array(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                "JSON-RPC batches are not supported",
            )
                .into_response();
        }
        _ => return (StatusCode::BAD_REQUEST, "body is not a JSON-RPC message").into_response(),
    };
    let id = message.get("id").map(Value::to_string);
    let method = message.get("method").and_then(Value::as_str);
    if id.is_none() && method.is_none() {
        return (StatusCode::BAD_REQUEST, "body is not a JSON-RPC message").into_response();
    }
    let is_initialize = method == Some("initialize") && id.is_some();

    let (sid, mut new_session) = match (session_id(&headers), is_initialize) {
        (Some(_), true) => {
            return (
                StatusCode::BAD_REQUEST,
                "initialize must not have an Mcp-Session-Id header",
            )
                .into_response();
        }
        (Some(sid), false) => (sid, None),
        (None, true) => match st.bridge.open(&name).await {
            Ok(sid) => (
                sid.clone(),
                Some(NewSession {
                    bridge: st.bridge.clone(),
                    sid,
                    name: name.clone(),
                    handed_over: false,
                }),
            ),
            Err(e) => return error_response(e),
        },
        (None, false) => {
            return (StatusCode::BAD_REQUEST, "Mcp-Session-Id header is missing").into_response();
        }
    };

    // Send one line of JSON, as the stdio transport needs.
    let line = message.to_string();
    let result = match id.filter(|_| method.is_some()) {
        Some(id) => st.bridge.request(&sid, &name, id, line).await.map(sse),
        // A notification or a response to a server request.
        None => st
            .bridge
            .send(&sid, &name, line)
            .await
            .map(|()| StatusCode::ACCEPTED.into_response()),
    };
    match result {
        Ok(mut response) => {
            if let Some(new) = new_session.as_mut()
                && let Ok(value) = HeaderValue::from_str(&sid)
            {
                response.headers_mut().insert(SESSION_HEADER, value);
                new.handed_over = true;
            }
            response
        }
        // On error, the drop of `new_session` ends the new session.
        Err(e) => error_response(e),
    }
}

/// A new session that the client does not know yet. If the handler stops
/// before the response has the session ID (an error, or the client went
/// away), the drop ends the session. Otherwise the session stays open with
/// nobody to end it.
struct NewSession {
    bridge: Bridge,
    sid: String,
    name: String,
    /// True after the session ID is in the response.
    handed_over: bool,
}

impl Drop for NewSession {
    fn drop(&mut self) {
        if !self.handed_over {
            self.bridge.close_later(
                std::mem::take(&mut self.sid),
                std::mem::take(&mut self.name),
            );
        }
    }
}

/// Open the standalone event stream of a session.
async fn listen(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !st.names.contains(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !accepts(&headers, &["text/event-stream"]) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must list text/event-stream",
        )
            .into_response();
    }
    let Some(sid) = session_id(&headers) else {
        return (StatusCode::BAD_REQUEST, "Mcp-Session-Id header is missing").into_response();
    };
    st.bridge
        .listen(&sid, &name)
        .await
        .map_or_else(error_response, sse)
}

/// End a session.
async fn delete(
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !st.names.contains(&name) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(sid) = session_id(&headers) else {
        return (StatusCode::BAD_REQUEST, "Mcp-Session-Id header is missing").into_response();
    };
    st.bridge
        .close(&sid, &name)
        .await
        .map_or_else(error_response, |()| StatusCode::OK.into_response())
}

/// Refuse requests from web pages of other origins. A request with no
/// `Origin` header does not come from a web page, so it can continue.
async fn check_origin(req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(ORIGIN) {
        let allowed = origin
            .to_str()
            .ok()
            .and_then(|o| o.split_once("://"))
            .map(|(_, rest)| host_name(rest.split('/').next().unwrap_or("")))
            .is_some_and(|host| ALLOWED_ORIGINS.contains(&host.as_str()));
        if !allowed {
            return (StatusCode::FORBIDDEN, "origin is not allowed").into_response();
        }
    }
    next.run(req).await
}

/// Make an event stream response that sends each message of `rx` as one
/// event.
fn sse(rx: MessageStream) -> Response {
    let events = futures::stream::unfold(rx, |mut rx| async move {
        let message = rx.recv().await?;
        Some((Ok::<_, Infallible>(Event::default().data(message)), rx))
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE))
        .into_response()
}

/// Map a bridge error to an HTTP error response.
fn error_response(err: BridgeError) -> Response {
    match err {
        BridgeError::UnknownSession => (StatusCode::NOT_FOUND, "session not found").into_response(),
        BridgeError::StreamOpen => {
            (StatusCode::CONFLICT, "session already has an event stream").into_response()
        }
        BridgeError::DuplicateId => {
            (StatusCode::BAD_REQUEST, "a request with this ID is open").into_response()
        }
        BridgeError::Host(e) => {
            // Log the detail. The client gets a fixed text.
            warn!("mcp: host call failed: {e}");
            (StatusCode::BAD_GATEWAY, "the MCP server is not available").into_response()
        }
    }
}

/// Get the session ID from the request headers.
fn session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Check that the `Accept` headers list all `types`. The spec requires that
/// the client lists the types, so a substring match is enough. Wildcards
/// such as `*/*` do not count.
fn accepts(headers: &HeaderMap, types: &[&str]) -> bool {
    let accept = headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",")
        .to_ascii_lowercase();
    types.iter().all(|t| accept.contains(t))
}
