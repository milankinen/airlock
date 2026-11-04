//! Generic JSON-RPC and ACP message helpers of the proxy.
//!
//! Holds the JSON-RPC message kinds, error codes and message builders. Also
//! maps the session dirs of the agent to host paths and adds a login hint
//! to the login errors of the agent.

use serde_json::{Value, json};

use crate::messages;

/// JSON-RPC id of the handshake request that the proxy sends to the agent.
pub const INIT_ID: &str = "airlock-acp-initialize";

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
        (None, Some(Value::String(method))) => Some(Kind::Notification { method }),
        (Some(id), None) => Some(Kind::Response { id }),
        _ => None,
    }
}

/// Change the guest session dirs in a `session/list` response to host
/// paths.
/// Args:
///  - `msg`: Agent response. Changed in place.
///  - `host_cwd`: Session dir of the request on the host
///  - `guest_cwd`: The same dir in the guest
///
/// Returns:
///   `true` if the message changed.
// Only the request dir and the paths in it are known on the host. Other
// paths stay as they are.
pub fn map_session_list(msg: &mut Value, host_cwd: &str, guest_cwd: &str) -> bool {
    let Some(sessions) = msg
        .pointer_mut("/result/sessions")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for cwd in sessions.iter_mut().filter_map(|s| s.get_mut("cwd")) {
        if let Some(host) = cwd
            .as_str()
            .and_then(|path| host_path(path, host_cwd, guest_cwd))
        {
            *cwd = Value::String(host);
            changed = true;
        }
    }
    changed
}

/// Get the host path of the guest path `path`.
/// Returns:
///   The host path, or `None` if `path` is not `guest_cwd` or in it, or
///   has a `..` part.
fn host_path(path: &str, host_cwd: &str, guest_cwd: &str) -> Option<String> {
    if path.split('/').any(|part| part == "..") {
        return None;
    }
    if path == guest_cwd {
        return Some(host_cwd.to_string());
    }
    // The match is per path part: `/work2` is not in `/work`.
    let rest = path.strip_prefix(guest_cwd.trim_end_matches('/'))?;
    if !rest.starts_with('/') {
        return None;
    }
    Some(format!("{}{rest}", host_cwd.trim_end_matches('/')))
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

/// Make a JSON-RPC error response.
pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Make a JSON-RPC error response with the error `data`.
pub fn error_response_with_data(id: &Value, code: i64, message: &str, data: &Value) -> Value {
    let mut msg = error_response(id, code, message);
    msg["error"]["data"] = data.clone();
    msg
}
