//! Generic JSON-RPC and ACP message helpers of the proxy.
//!
//! Holds the JSON-RPC message kinds, error codes and message builders. Also
//! maps the session dirs of the agent to host paths and adds a login hint
//! to the login errors of the agent.

use serde_json::{Value, json};

use crate::messages;

/// JSON-RPC id of the handshake request that the proxy sends to the agent.
pub const INIT_ID: &str = "airlock-acp-initialize";

/// Get the JSON-RPC id of the request that opens the ACP session of the
/// session handle `hid` again in a new agent. The client does not see it.
// The string form keeps it apart from the numeric ids of the editors.
pub fn reopen_id(hid: u64) -> String {
    format!("airlock-acp-reopen-{hid}")
}

/// Get the id for the client of an agent request: `<hid>.<n>`.
/// Args:
///  - `hid`: The session handle of the agent
///  - `n`: Number of the request in the handle, over all its agents
pub fn agent_request_id(hid: u64, n: u64) -> String {
    format!("{hid}.{n}")
}

/// Get the session handle in the id of an agent request for the client.
/// Returns:
///   The handle, or `None` if the id is not an agent request id.
pub fn agent_request_handle(id: &Value) -> Option<u64> {
    id.as_str()?.split_once('.')?.0.parse().ok()
}

/// Maximum length of an agent request id or a new session id from the
/// agent, in bytes. The proxy keeps them in memory.
pub const MAX_ID_BYTES: usize = 256;

/// ACP protocol version that the proxy supports.
pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC error code for a request that is not valid at this time.
pub const INVALID_REQUEST: i64 = -32600;

/// JSON-RPC error code for a method that is not available.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC error code for params that are not valid.
pub const INVALID_PARAMS: i64 = -32602;

/// JSON-RPC error code for an internal error.
pub const INTERNAL_ERROR: i64 = -32603;

/// ACP error code: the agent needs a login before it can do the request.
pub const AUTH_REQUIRED: i64 = -32000;

/// Method of the notification that cancels a request.
pub const CANCEL_REQUEST: &str = "$/cancel_request";

/// Map key of a JSON-RPC request id: the id as JSON text. A response has
/// the same key as its request.
pub type RequestId = String;

/// ACP session id.
pub type SessionId = String;

/// Get the map key of the request id `req_id`.
pub fn key(req_id: &Value) -> RequestId {
    req_id.to_string()
}

/// Get the session id in the params of `msg`, if it has one.
pub fn session_id(msg: &Value) -> Option<&str> {
    msg.pointer("/params/sessionId").and_then(Value::as_str)
}

/// Check that the session ids in `params.sessionId` and
/// `result.sessionId` of the agent message `msg` are strings.
/// Returns:
///   `false` if one of them is not a string. The message is then not
///   valid for the client.
pub fn session_ids_are_strings(msg: &Value) -> bool {
    ["/params/sessionId", "/result/sessionId"]
        .iter()
        .all(|path| msg.pointer(path).is_none_or(Value::is_string))
}

/// Encode a JSON-RPC message as one line with its newline.
pub fn to_line(msg: &Value) -> Vec<u8> {
    let mut line = msg.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// Kind of a JSON-RPC message.
pub enum Kind<'a> {
    /// Request with an id and a method.
    Request {
        /// Id of the request.
        id: &'a Value,
        /// Method of the request.
        method: &'a str,
    },
    /// Notification: a method without an id.
    Notification {
        /// Method of the notification.
        method: &'a str,
    },
    /// `$/cancel_request` notification that cancels the request `req_id`.
    CancelRequest {
        /// Id of the request to cancel, or `None` if the params have no
        /// request id.
        req_id: Option<&'a Value>,
    },
    /// Response to a request.
    Response {
        /// Id of the request.
        id: &'a Value,
    },
}

/// Get the kind of a JSON-RPC message.
/// Returns:
///   The kind, or `None` if the value is not a JSON-RPC message.
// Only a message without a method is a response. A method that is not a
// string makes the message not valid. The filter must not forward it as a
// response.
pub fn kind(msg: &Value) -> Option<Kind<'_>> {
    match (msg.get("id"), msg.get("method")) {
        (Some(id), Some(Value::String(method))) => Some(Kind::Request { id, method }),
        (None, Some(Value::String(method))) if method == CANCEL_REQUEST => {
            Some(Kind::CancelRequest {
                req_id: msg.pointer("/params/requestId"),
            })
        }
        (None, Some(Value::String(method))) => Some(Kind::Notification { method }),
        (Some(id), None) => Some(Kind::Response { id }),
        _ => None,
    }
}

/// Change the guest session dirs in a `session/list` response to host
/// paths: the `cwd` and the `additionalDirectories` of each row.
/// Args:
///  - `msg`: Agent response. Changed in place.
///  - `host_root`: Project dir on the host
///  - `guest_root`: Project dir in the guest
///
/// Returns:
///   `true` if the message changed.
// Only the paths in the project dir have a different path on the host.
// Other dirs, for example a git worktree, have the same path in the guest
// and on the host. Thus they stay as they are.
pub fn map_session_list(msg: &mut Value, host_root: &str, guest_root: &str) -> bool {
    let Some(sessions) = msg
        .pointer_mut("/result/sessions")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    let mut map = |dir: &mut Value| {
        if let Some(host) = dir
            .as_str()
            .and_then(|path| host_path(path, host_root, guest_root))
        {
            *dir = Value::String(host);
            changed = true;
        }
    };
    for row in sessions.iter_mut() {
        if let Some(cwd) = row.get_mut("cwd") {
            map(cwd);
        }
        if let Some(dirs) = row
            .get_mut("additionalDirectories")
            .and_then(Value::as_array_mut)
        {
            dirs.iter_mut().for_each(&mut map);
        }
    }
    changed
}

/// Get the host path of the guest path `path`.
/// Returns:
///   The host path, or `None` if `path` is not `guest_root` or in it, or
///   has a `..` part.
fn host_path(path: &str, host_root: &str, guest_root: &str) -> Option<String> {
    if path.split('/').any(|part| part == "..") {
        return None;
    }
    // The match is per path part: `/work2` is not in `/work`. A root `/`
    // becomes empty, and then each absolute path is in it.
    let rest = path.strip_prefix(guest_root.trim_end_matches('/'))?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let host = format!(
        "{}{}",
        host_root.trim_end_matches('/'),
        rest.trim_end_matches('/')
    );
    Some(if host.is_empty() { "/".into() } else { host })
}

/// Add a login hint to an `auth_required` error response of the agent.
/// Args:
///  - `msg`: Agent message. Changed in place.
///  - `agent_cmd`: Agent command, for the hint
///
/// Returns:
///   `true` if the message changed.
// The proxy shows no login methods to the client (`authMethods` is empty),
// and the login terminal is not given to the agent. Thus the user must log
// in inside the sandbox.
pub fn add_auth_hint(msg: &mut Value, agent_cmd: &str) -> bool {
    if msg.get("method").is_some()
        || msg.pointer("/error/code").and_then(Value::as_i64) != Some(AUTH_REQUIRED)
    {
        return false;
    }
    let text = msg
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = messages::acp::auth_hint(text, agent_cmd);
    msg["error"]["message"] = json!(text);
    true
}

/// Make a JSON-RPC request.
pub fn request(id: &str, method: &str, params: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Make a JSON-RPC success response.
pub fn result_response(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Make a JSON-RPC error response of the proxy. The message gets the
/// `airlock: ` prefix.
// The agent can show the errors that it gets to the user. The prefix
// tells the user that the error is from airlock.
pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    let message = messages::acp::from_airlock(message);
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Make a JSON-RPC error response of the proxy with the error `data` (see
/// [`error_response`]).
pub fn error_response_with_data(id: &Value, code: i64, message: &str, data: &Value) -> Value {
    let mut msg = error_response(id, code, message);
    msg["error"]["data"] = data.clone();
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the host path of a guest path is per path part, with
    /// roots that end in a slash and with the root `/`. The result must not
    /// get a double slash or a slash at the end.
    ///   1. Map paths in the root, the root itself and paths out of it
    ///   2. Map with a guest root `/` and a host root `/`
    ///   3. Check that `..` paths and paths with the same prefix stay
    #[test]
    fn host_path_maps_per_path_part() {
        let cases = [
            ("/work/x", "/projects/a", "/work", Some("/projects/a/x")),
            ("/work", "/projects/a", "/work", Some("/projects/a")),
            ("/work/", "/projects/a/", "/work/", Some("/projects/a")),
            ("/work/x/", "/projects/a/", "/work/", Some("/projects/a/x")),
            ("/work2", "/projects/a", "/work", None),
            ("/work/../etc", "/projects/a", "/work", None),
            ("/other", "/projects/a", "/work", None),
            ("/x/y", "/projects/a", "/", Some("/projects/a/x/y")),
            ("/", "/projects/a", "/", Some("/projects/a")),
            ("/work/x", "/", "/work", Some("/x")),
            ("/work", "/", "/work", Some("/")),
            ("x", "/projects/a", "/", None),
        ];
        for (path, host, guest, expected) in cases {
            assert_eq!(
                host_path(path, host, guest).as_deref(),
                expected,
                "{path} {host} {guest}"
            );
        }
    }

    /// Test that `kind` finds each `$/cancel_request` as a cancel request,
    /// with the request id of its params if it has one.
    ///   1. Get the kind of a `$/cancel_request` with `params.requestId`
    ///   2. Get the kind of a `$/cancel_request` without it
    ///   3. Check that both are `CancelRequest`, the second without an id
    #[test]
    fn cancel_request_kind_has_optional_request_id() {
        let with_id = json!({
            "jsonrpc": "2.0", "method": "$/cancel_request", "params": { "requestId": 7 }
        });
        let Some(Kind::CancelRequest { req_id }) = kind(&with_id) else {
            panic!("expected a cancel request");
        };
        assert_eq!(req_id, Some(&json!(7)));
        let without_id = json!({ "jsonrpc": "2.0", "method": "$/cancel_request", "params": {} });
        assert!(matches!(
            kind(&without_id),
            Some(Kind::CancelRequest { req_id: None })
        ));
    }
}
