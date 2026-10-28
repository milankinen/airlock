//! Message rules of the ACP proxy.
//!
//! Makes the handshake reply that the proxy gives before the agent runs.
//! Removes the client features that let the agent act on the host, and
//! lets through only the agent messages that the remaining features use.
//! Also refuses the client requests for features that the real agent does
//! not have.

use serde_json::{Map, Value, json};

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

/// Agent requests that the client gets in restrict mode. A form prompt
/// (`elicitation/create`) has a separate check.
const AGENT_REQUESTS: &[&str] = &["session/request_permission"];

/// Agent notifications that the client gets in restrict mode.
const AGENT_NOTIFICATIONS: &[&str] =
    &["session/update", "elicitation/complete", "$/cancel_request"];

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

/// Make the handshake reply that the proxy gives to the client.
/// Args:
///  - `title`: Name of the agent that the client shows
///  - `version`: Version of airlock
///  - `proxy_mcp_servers`: The proxy bridges the editor MCP servers
///
/// Returns:
///   The `initialize` result.
// The client waits for this reply before it sends the session request that
// has the project dir. Thus the proxy replies before the agent runs. The
// reply has only features that the common agents support. When the real
// agent runs, the proxy refuses the client requests that use a feature
// that the agent does not have (see `check_client_request`).
//
// The proxy itself serves the editor MCP servers to the agent over HTTP.
// Thus it can take HTTP and SSE servers also when the agent has only HTTP.
pub fn initialize_result(title: &str, version: &str, proxy_mcp_servers: bool) -> Value {
    let mut result = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": { "image": true, "embeddedContext": true },
        },
        "authMethods": [],
        "agentInfo": { "name": "airlock", "title": title, "version": version },
    });
    if proxy_mcp_servers {
        result["agentCapabilities"]["mcpCapabilities"] = json!({ "http": true, "sse": true });
    }
    result
}

/// Make the `initialize` params for the agent from the client params.
/// Args:
///  - `params`: Client params. Changed in place.
///  - `restrict`: Also remove the client features that let the agent act
///    on the host
// The proxy replied with version 1 to the client. Thus the agent must use
// version 1 too, and the params must not have the fields of other versions.
pub fn prepare_initialize_params(params: &mut Value, restrict: bool) {
    if !params.is_object() {
        *params = json!({});
    }
    params["protocolVersion"] = json!(PROTOCOL_VERSION);
    if let Some(params) = params.as_object_mut() {
        params.remove("capabilities");
    }
    if restrict {
        restrict_client_capabilities(params);
    }
}

/// Keep only the client features that do not let the agent act on the
/// host: form prompts, session features and login features without the
/// login terminal. Removes all other features, also unknown ones.
fn restrict_client_capabilities(params: &mut Value) {
    let Some(caps) = params
        .get_mut("clientCapabilities")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let mut kept = Map::new();
    if let Some(form) = caps.get("elicitation").and_then(|e| e.get("form")) {
        kept.insert("elicitation".into(), json!({ "form": form }));
    }
    if let Some(session) = caps.get("session") {
        kept.insert("session".into(), session.clone());
    }
    if let Some(mut auth) = caps.get("auth").and_then(Value::as_object).cloned() {
        auth.remove("terminal");
        kept.insert("auth".into(), Value::Object(auth));
    }
    *caps = kept;
}

/// What the proxy does with an agent message in restrict mode.
pub enum AgentVerdict {
    /// Send the message to the client.
    Forward,
    /// Reply to the agent request with an error and do not send it.
    Refuse { code: i64, message: String },
    /// Do not send the message.
    Drop,
}

/// Check an agent message in restrict mode.
/// Returns:
///   What the proxy does with the message.
pub fn check_agent_message(msg: &Value) -> AgentVerdict {
    match kind(msg) {
        Some(Kind::Response { .. }) => AgentVerdict::Forward,
        Some(Kind::Request { method, .. }) if AGENT_REQUESTS.contains(&method) => {
            AgentVerdict::Forward
        }
        Some(Kind::Request {
            method: "elicitation/create",
            ..
        }) => {
            if msg.pointer("/params/mode").and_then(Value::as_str) == Some("form") {
                AgentVerdict::Forward
            } else {
                AgentVerdict::Refuse {
                    code: INVALID_PARAMS,
                    message: messages::acp::REFUSED_ELICITATION.into(),
                }
            }
        }
        Some(Kind::Request { method, .. }) => AgentVerdict::Refuse {
            code: METHOD_NOT_FOUND,
            message: messages::acp::blocked_request(method),
        },
        Some(Kind::Notification { method }) if AGENT_NOTIFICATIONS.contains(&method) => {
            AgentVerdict::Forward
        }
        Some(Kind::Notification { .. }) | None => AgentVerdict::Drop,
    }
}

/// Check a client request against the features of the agent.
/// Args:
///  - `method`: Method of the request
///  - `params`: Params of the request
///  - `agent`: `initialize` result of the agent
///
/// Returns:
///   `Ok`, or the error code and message for a request that uses a feature
///   that the agent does not have.
// The proxy told the client that the agent has these features before the
// agent ran (see `initialize_result`).
pub fn check_client_request(
    method: &str,
    params: &Value,
    agent: &Value,
) -> Result<(), (i64, String)> {
    let has = |pointer: &str| agent.pointer(pointer).and_then(Value::as_bool) == Some(true);
    if method == "session/load" && !has("/agentCapabilities/loadSession") {
        return Err((METHOD_NOT_FOUND, messages::acp::NO_LOAD_SESSION.into()));
    }
    if method == "session/prompt" {
        let blocks = params.get("prompt").and_then(Value::as_array);
        for block in blocks.into_iter().flatten() {
            let block_type = block.get("type").and_then(Value::as_str);
            let missing = match block_type {
                Some("image") => !has("/agentCapabilities/promptCapabilities/image"),
                Some("audio") => !has("/agentCapabilities/promptCapabilities/audio"),
                Some("resource") => !has("/agentCapabilities/promptCapabilities/embeddedContext"),
                _ => false,
            };
            if let (true, Some(block_type)) = (missing, block_type) {
                return Err((
                    INVALID_PARAMS,
                    messages::acp::missing_prompt_content(block_type),
                ));
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    //! Tests for the client feature rules.

    use super::*;

    /// Test that the proxy keeps only the allowed client features, and
    /// makes the params of version 1.
    ///   1. Prepare the params of a client that has all features, unknown
    ///      features and the fields of an other version
    ///   2. Check that only form prompts, session features and login
    ///      features without the login terminal stay, and the version is 1
    #[test]
    fn restricted_initialize_params_keep_only_allowed_features() {
        let mut params = json!({
            "protocolVersion": 2,
            "capabilities": { "fs": {} },
            "clientInfo": { "name": "editor" },
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true,
                "auth": { "terminal": true, "other": {} },
                "elicitation": { "form": {}, "url": {} },
                "session": { "notices": {} },
                "_meta": { "x": 1 },
                "future": true,
            },
        });
        prepare_initialize_params(&mut params, true);
        assert_eq!(
            params,
            json!({
                "protocolVersion": 1,
                "clientInfo": { "name": "editor" },
                "clientCapabilities": {
                    "auth": { "other": {} },
                    "elicitation": { "form": {} },
                    "session": { "notices": {} },
                },
            })
        );
    }

    /// Test that the proxy refuses the client requests for agent features
    /// that the real agent does not have, and lets the others go.
    ///   1. Use an agent with no features
    ///   2. Check that session load, and image, audio and embedded prompt
    ///      content are refused
    ///   3. Check that a text prompt and a session with MCP servers are
    ///      allowed. The proxy bridges the servers itself
    #[test]
    fn client_requests_for_missing_agent_features_are_refused() {
        let agent = json!({ "protocolVersion": 1, "agentCapabilities": {} });
        let refused = |method: &str, params: Value| {
            check_client_request(method, &params, &agent).unwrap_err().0
        };
        assert_eq!(refused("session/load", json!({})), METHOD_NOT_FOUND);
        for block in ["image", "audio", "resource"] {
            let prompt = json!({ "prompt": [{ "type": block }] });
            assert_eq!(refused("session/prompt", prompt), INVALID_PARAMS);
        }
        let ok = json!({
            "prompt": [{ "type": "text", "text": "hi" }],
            "mcpServers": [
                { "name": "local", "command": "x" },
                { "type": "sse", "name": "remote", "url": "http://x" },
            ],
        });
        assert!(check_client_request("session/prompt", &ok, &agent).is_ok());
        assert!(check_client_request("session/new", &ok, &agent).is_ok());
    }
}
