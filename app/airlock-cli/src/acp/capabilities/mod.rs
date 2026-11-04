//! Agent features that the proxy offers to the client, and the client
//! features that the agent gets.
//!
//! The proxy replies to the client handshake before the agent runs. Thus
//! it does not know the features of the real agent at that time. A feature
//! set tells which agent features the reply offers. The common agents each
//! have a preset set, and the user can give a different set. The feature
//! names are the paths of the ACP fields, and `air:<name>` for the
//! JetBrains AIR features.
//!
//! Also tells which agent feature a client request needs, and which
//! offered features the real agent does not have.
//!
//! The standard ACP rules and each agent and each client have their own
//! sub-module. This module only combines them. In restrict mode the agent
//! gets only the client features that do not let it act on the host.

mod agent_claude;
mod agent_codex;
mod agent_copilot;
mod client_jetbrains;
mod client_zed;
mod standard;

use serde_json::Value;

use crate::acp::filter::{self, Kind};
use crate::messages;

/// Features of the preset for other agents.
const OTHER: &[&str] = &[
    "loadSession",
    "promptCapabilities.image",
    "promptCapabilities.embeddedContext",
    "steering",
];

/// All feature names that a set can have.
fn known() -> impl Iterator<Item = String> {
    standard::known().chain(client_jetbrains::known())
}

/// Get the preset feature set of the agent command `argv`.
/// Args:
///  - `argv`: Agent command and its arguments, before the login shell
///    wrap
///
/// Returns:
///   The feature names.
// The match uses the last name of the command. A launcher such as `npx`
// gets the preset for other agents.
//
// The presets have no logout. A logout in one sandbox revokes the shared
// sign-in of all sandboxes that use the agent pack.
pub fn preset(argv: &[String]) -> Vec<String> {
    agent_claude::preset(argv)
        .or_else(|| agent_codex::preset(argv))
        .or_else(|| agent_copilot::preset(argv))
        .unwrap_or_else(|| OTHER.iter().map(|n| (*n).to_string()).collect())
}

/// Get the last name of the agent command.
/// Args:
///  - `argv`: Agent command and its arguments
fn command_name(argv: &[String]) -> &str {
    argv.first()
        .map_or("", |cmd| cmd.rsplit('/').next().unwrap_or(""))
}

/// Make a list of feature names from static names.
fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| (*n).to_string()).collect()
}

/// Parse a comma-separated list of feature names.
/// Returns:
///   The names, or the names that are not known. An empty list gives no
///   names.
pub fn parse(text: &str) -> Result<Vec<String>, Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut unknown = Vec::new();
    for name in text.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !known().any(|k| k == name) {
            unknown.push(name.to_string());
        } else if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    if unknown.is_empty() {
        Ok(names)
    } else {
        Err(unknown)
    }
}

/// All feature names that a set can have, comma separated, for messages.
pub fn valid_names() -> String {
    known().collect::<Vec<_>>().join(", ")
}

/// Add the features of a set to the handshake reply.
/// Args:
///  - `result`: `initialize` result
///  - `names`: Feature names of the set
///  - `client`: Client `initialize` params after the filter
///
/// Returns:
///   The `initialize` result with the features added.
fn offer(result: Value, names: &[String], client: &Value) -> Value {
    client_jetbrains::offer(standard::offer(result, names), names, client)
}

/// Test if a feature is present in an `initialize` result. A feature is
/// present when its value is `true` or an object.
/// Args:
///  - `init`: `initialize` result
///  - `pointer`: JSON pointer of the feature
fn has(init: &Value, pointer: &str) -> bool {
    matches!(
        init.pointer(pointer),
        Some(Value::Bool(true) | Value::Object(_))
    )
}

/// Test if the feature `name` is present in an `initialize` result. Also
/// takes the AIR names of the session index extras.
fn present(init: &Value, name: &str) -> bool {
    client_jetbrains::present(init, name)
        .or_else(|| standard::present(init, name))
        .unwrap_or(false)
}

/// Get the feature that a client request needs from the agent.
/// Returns:
///   The feature name, or `None` if the request needs no feature.
fn required(method: &str) -> Option<&'static str> {
    standard::required(method).or_else(|| client_jetbrains::required(method))
}

/// Get the features that the proxy offered but the real agent does not
/// have.
/// Args:
///  - `offered`: `initialize` result of the proxy
///  - `agent`: `initialize` result of the real agent
pub fn missing(offered: &Value, agent: &Value) -> Vec<String> {
    known()
        .chain(client_jetbrains::extra_names())
        .filter(|name| present(offered, name) && !present(agent, name))
        .collect()
}

/// Make the `initialize` params for the agent from the client params.
/// Args:
///  - `params`: Client params
///  - `restrict`: Also remove the client features that let the agent act
///    on the host
///
/// Returns:
///   The params for the agent.
// The proxy replied with version 1 to the client. Thus the agent must use
// version 1 too, and the params must not have the fields of other versions.
pub fn prepare_initialize_params(params: Value, restrict: bool) -> Value {
    let params = standard::prepare_params(params);
    if restrict {
        restrict_client_capabilities(params)
    } else {
        params
    }
}

/// Keep only the client features that do not let the agent act on the
/// host: the standard safe features and the safe client specific `_meta`
/// features. Removes all other features, also unknown ones.
/// Args:
///  - `params`: Client params
///
/// Returns:
///   The params with only the kept client features.
fn restrict_client_capabilities(mut params: Value) -> Value {
    let Some(caps) = params.get("clientCapabilities").and_then(Value::as_object) else {
        return params;
    };
    let mut kept = standard::keep_caps(caps);
    if let Some(meta) = caps.get("_meta").and_then(Value::as_object) {
        let mut kept_meta = client_zed::keep_meta(meta);
        kept_meta.extend(client_jetbrains::keep_meta(meta));
        kept_meta.extend(agent_claude::keep_meta(meta));
        if !kept_meta.is_empty() {
            kept.insert("_meta".into(), Value::Object(kept_meta));
        }
    }
    params["clientCapabilities"] = Value::Object(kept);
    params
}

/// Make the handshake reply that the proxy gives to the client.
/// Args:
///  - `title`: Name of the agent that the client shows
///  - `version`: Version of airlock
///  - `proxy_mcp_servers`: The proxy bridges the editor MCP servers
///  - `features`: Agent feature set that the reply offers
///  - `client`: Client `initialize` params after the filter
///
/// Returns:
///   The `initialize` result.
// The client waits for this reply before it sends the session request that
// has the project dir. Thus the proxy replies before the agent runs. The
// reply has the features of the set. When the real agent runs, the proxy
// refuses the client requests that use a feature that the agent does not
// have (see `check_client_request`).
//
// The proxy shows no login methods. The user logs in inside the sandbox.
pub fn initialize_result(
    title: &str,
    version: &str,
    proxy_mcp_servers: bool,
    features: &[String],
    client: &Value,
) -> Value {
    let result = standard::initialize_result(title, version);
    let result = offer(result, features, client);
    standard::offer_mcp(result, proxy_mcp_servers)
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
    match filter::kind(msg) {
        Some(Kind::Response { .. }) => AgentVerdict::Forward,
        Some(Kind::Request { method, .. }) => match standard::check_agent_request(method, msg) {
            Some(Ok(())) => AgentVerdict::Forward,
            Some(Err((code, message))) => AgentVerdict::Refuse { code, message },
            None => AgentVerdict::Refuse {
                code: filter::METHOD_NOT_FOUND,
                message: messages::acp::blocked_request(method),
            },
        },
        Some(Kind::Notification { method }) if is_agent_notification(method) => {
            AgentVerdict::Forward
        }
        Some(Kind::Notification { .. }) | None => AgentVerdict::Drop,
    }
}

/// Test if the client gets an agent notification in restrict mode.
fn is_agent_notification(method: &str) -> bool {
    [
        standard::AGENT_NOTIFICATIONS,
        agent_claude::AGENT_NOTIFICATIONS,
        agent_codex::AGENT_NOTIFICATIONS,
        client_jetbrains::AGENT_NOTIFICATIONS,
    ]
    .iter()
    .any(|list| list.contains(&method))
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
    if let Some(feature) = required(method)
        && !present(agent, feature)
    {
        return Err((
            filter::METHOD_NOT_FOUND,
            messages::acp::unsupported_method(method),
        ));
    }
    let has_prompt = standard::PROMPT_METHODS.contains(&method)
        || client_jetbrains::PROMPT_METHODS.contains(&method);
    standard::check_prompt(params.get("prompt").filter(|_| has_prompt), agent)
}

#[cfg(test)]
mod tests {
    //! Tests for the agent feature sets.

    use serde_json::json;

    use super::*;
    use crate::acp::filter::{INVALID_PARAMS, METHOD_NOT_FOUND};

    /// Test that the presence test takes `true` and objects, and that the
    /// offered features that the agent does not have are found. Most agent
    /// features are empty objects, not `true`.
    ///   1. Make an agent result with `{}`, `true`, `false` and AIR values
    ///   2. Check the presence of each feature
    ///   3. Check the missing features against the Claude preset
    #[test]
    fn presence_test_takes_empty_objects_and_finds_missing_features() {
        let agent = json!({
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": false },
                "sessionCapabilities": { "list": {}, "fork": null },
            },
            "_meta": { "jetbrains": { "air": { "capabilities": ["diffPatch"] } } },
        });
        assert!(present(&agent, "loadSession"));
        assert!(present(&agent, "sessionCapabilities.list"));
        assert!(!present(&agent, "sessionCapabilities.fork"));
        assert!(!present(&agent, "promptCapabilities.image"));
        assert!(present(&agent, "air:diffPatch"));
        assert!(!present(&agent, "air:goal"));

        let client = json!({ "clientCapabilities": { "_meta": { "jetbrains": { "air": {} } } } });
        let offered = offer(json!({}), &preset(&["claude-agent-acp".into()]), &client);
        let missing = missing(&offered, &agent);
        assert!(missing.contains(&"sessionCapabilities.fork".to_string()));
        assert!(missing.contains(&"air:goal".to_string()));
        assert!(!missing.contains(&"loadSession".to_string()));
        assert!(!missing.contains(&"air:diffPatch".to_string()));
    }

    /// Test that the presets follow the agent command name, and that an
    /// unknown name in a list is refused.
    ///   1. Get the presets of a Claude path, of Copilot with and without
    ///      `--acp`, and of a launcher
    ///   2. Parse a list with an unknown name and an empty list
    #[test]
    fn presets_follow_command_name_and_parse_refuses_unknown_names() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let claude = preset(&argv(&["/usr/bin/claude-agent-acp"]));
        assert!(claude.contains(&"air:backgroundSubagents".to_string()));
        assert_eq!(
            preset(&argv(&["copilot", "--acp"])),
            argv(agent_copilot::FEATURES)
        );
        assert_eq!(preset(&argv(&["copilot"])), argv(OTHER));
        assert_eq!(preset(&argv(&["npx", "claude-agent-acp"])), argv(OTHER));

        assert_eq!(
            parse("loadSession, nope,air:x"),
            Err(argv(&["nope", "air:x"]))
        );
        assert_eq!(parse(""), Ok(Vec::new()));
    }

    /// Test that the proxy keeps only the allowed client features, and
    /// makes the params of version 1.
    ///   1. Prepare the params of a client that has all features, unknown
    ///      features and the fields of an other version
    ///   2. Check that only form prompts and session features stay, and the
    ///      version is 1
    #[test]
    fn restricted_initialize_params_keep_only_allowed_features() {
        let params = json!({
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
        let params = prepare_initialize_params(params, true);
        assert_eq!(
            params,
            json!({
                "protocolVersion": 1,
                "clientInfo": { "name": "editor" },
                "clientCapabilities": {
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
