//! Standard ACP protocol rules.
//!
//! Holds the rules that all agents and clients share. Tells which agent
//! messages the client may get, which client features the agent may get,
//! which agent feature each client request needs, and which prompt content
//! the agent must support.

use serde_json::{Map, Value, json};

use crate::acp::filter::{INVALID_PARAMS, PROTOCOL_VERSION};
use crate::messages;

/// Features that are `true` in the agent features of the reply.
const FLAGS: &[&str] = &[
    "loadSession",
    "promptCapabilities.image",
    "promptCapabilities.audio",
    "promptCapabilities.embeddedContext",
];

/// Features that are an empty object in the agent features of the reply.
const OBJECTS: &[&str] = &[
    "sessionCapabilities.list",
    "sessionCapabilities.resume",
    "sessionCapabilities.close",
    "sessionCapabilities.delete",
    "sessionCapabilities.fork",
    "sessionCapabilities.additionalDirectories",
    "sessionCapabilities.systemPrompt",
    "sessionCapabilities.subagents",
    "auth.logout",
    "providers",
    "_meta.authStatus",
];

/// All standard feature names that a set can have.
pub(super) fn known() -> impl Iterator<Item = String> {
    FLAGS.iter().chain(OBJECTS).map(|n| (*n).to_string())
}

/// Add the standard agent features of a set to the handshake reply.
/// Args:
///  - `result`: `initialize` result
///  - `names`: Feature names of the set. Other names are ignored.
///
/// Returns:
///   The `initialize` result with the standard features added.
pub(super) fn offer(mut result: Value, names: &[String]) -> Value {
    for name in names {
        let name = name.as_str();
        if FLAGS.contains(&name) || OBJECTS.contains(&name) {
            let value = if FLAGS.contains(&name) {
                json!(true)
            } else {
                json!({})
            };
            let mut target = &mut result["agentCapabilities"];
            for part in name.split('.') {
                target = &mut target[part];
            }
            *target = value;
        }
    }
    result
}

/// Test if a standard feature is present in an `initialize` result.
/// Returns:
///   `None` if the name is not a standard feature, else if it is present.
pub(super) fn present(init: &Value, name: &str) -> Option<bool> {
    (FLAGS.contains(&name) || OBJECTS.contains(&name)).then(|| {
        super::has(
            init,
            &format!("/agentCapabilities/{}", name.replace('.', "/")),
        )
    })
}

/// Make the client `initialize` params fit the protocol version of the
/// proxy.
/// Args:
///  - `params`: Client params
///
/// Returns:
///   The params for the agent.
// The proxy replied with version 1 to the client. Thus the agent must use
// version 1 too, and the params must not have the fields of other versions.
pub(super) fn prepare_params(mut params: Value) -> Value {
    if !params.is_object() {
        params = json!({});
    }
    params["protocolVersion"] = json!(PROTOCOL_VERSION);
    if let Some(map) = params.as_object_mut() {
        map.remove("capabilities");
    }
    params
}

/// Make the handshake reply without agent features.
/// Args:
///  - `title`: Name of the agent that the client shows
///  - `version`: Version of airlock
pub(super) fn initialize_result(title: &str, version: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {},
        "authMethods": [],
        "agentInfo": { "name": "airlock", "title": title, "version": version },
    })
}

/// Add the MCP server support of the proxy to the handshake reply.
/// Args:
///  - `result`: `initialize` result
///  - `proxy_mcp_servers`: The proxy bridges the editor MCP servers
///
/// Returns:
///   The `initialize` result with the MCP support added.
// The proxy itself serves the editor MCP servers to the agent over HTTP.
// Thus it can take HTTP and SSE servers also when the agent has only HTTP.
pub(super) fn offer_mcp(mut result: Value, proxy_mcp_servers: bool) -> Value {
    if proxy_mcp_servers {
        result["agentCapabilities"]["mcpCapabilities"] = json!({ "http": true, "sse": true });
    }
    result
}

/// Client request methods that have a prompt with content blocks.
pub(super) const PROMPT_METHODS: &[&str] = &["session/prompt"];

/// Agent notifications that the client gets in restrict mode.
pub(super) const AGENT_NOTIFICATIONS: &[&str] =
    &["session/update", "elicitation/complete", "$/cancel_request"];

/// Check an agent request in restrict mode. A form prompt is allowed. Any
/// other kind of prompt is refused.
/// Args:
///  - `method`: Method of the request
///  - `msg`: The request
///
/// Returns:
///   `None` if the method is not a standard request that is allowed in
///   restrict mode. Else `Ok`, or the error code and message to reply with.
pub(super) fn check_agent_request(method: &str, msg: &Value) -> Option<Result<(), (i64, String)>> {
    match method {
        "session/request_permission" => Some(Ok(())),
        "elicitation/create" => {
            if msg.pointer("/params/mode").and_then(Value::as_str) == Some("form") {
                Some(Ok(()))
            } else {
                Some(Err((
                    INVALID_PARAMS,
                    messages::acp::REFUSED_ELICITATION.into(),
                )))
            }
        }
        _ => None,
    }
}

/// Copy the standard client features that do not let the agent act on the
/// host: form prompts, session, sub-agent and plan features.
/// Args:
///  - `caps`: Client features
///
/// Returns:
///   The kept features.
pub(super) fn keep_caps(caps: &Map<String, Value>) -> Map<String, Value> {
    let mut kept = Map::new();
    if let Some(form) = caps.get("elicitation").and_then(|e| e.get("form")) {
        kept.insert("elicitation".into(), json!({ "form": form }));
    }
    for name in ["session", "subagents", "plan"] {
        if let Some(value) = caps.get(name) {
            kept.insert(name.into(), value.clone());
        }
    }
    kept
}

/// Get the standard agent feature that a client request needs.
/// Returns:
///   The feature name, or `None` if the request needs no standard feature.
pub(super) fn required(method: &str) -> Option<&'static str> {
    Some(match method {
        "session/load" => "loadSession",
        "session/list" => "sessionCapabilities.list",
        "session/resume" => "sessionCapabilities.resume",
        "session/close" => "sessionCapabilities.close",
        "session/delete" => "sessionCapabilities.delete",
        "session/fork" => "sessionCapabilities.fork",
        "logout" => "auth.logout",
        m if m.starts_with("providers/") => "providers",
        _ => return None,
    })
}

/// Check that the agent supports the content blocks of a prompt.
/// Args:
///  - `prompt`: Prompt of the request
///  - `agent`: `initialize` result of the agent
///
/// Returns:
///   `Ok`, or the error code and message for a block that the agent does
///   not support.
pub(super) fn check_prompt(prompt: Option<&Value>, agent: &Value) -> Result<(), (i64, String)> {
    let has = |pointer: &str| super::has(agent, pointer);
    for block in prompt.and_then(Value::as_array).into_iter().flatten() {
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
    Ok(())
}
