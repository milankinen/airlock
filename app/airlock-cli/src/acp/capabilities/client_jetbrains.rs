//! JetBrains specific client features.
//!
//! Handles the JetBrains AIR features, the steering feature and the
//! JetBrains terminal output feature. Also tells the client requests that
//! these features use. For the agent, keeps only the
//! AIR client features that are safe. For the client, adds the AIR
//! features of the agent feature set to the handshake reply, and finds
//! them in the reply of the real agent.

use serde_json::{Map, Value, json};

/// Prefix of the JetBrains AIR features.
pub(super) const PREFIX: &str = "air:";

/// AIR feature for the session goal requests.
pub(super) const GOAL: &str = "air:goal";

/// AIR names that the reply can have in its AIR list.
pub(super) const AGENT_NAMES: &[&str] = &[
    "sessionFailure",
    "nativeSubagentSessions",
    "asyncTasks",
    "customInstructions",
    "recommendedValue",
    "diffPatch",
    "rawInputRendering",
    "planContentDelta",
    "backgroundSubagents",
];

/// JetBrains AIR client features that the agent gets in restrict mode.
// Not in the list: `planFile` makes the client read the plan from a guest
// path on the host, and `agentFileChangeReport` lets the client act on
// host paths. The session index has requests without a session id, and
// each session has its own agent.
const AIR_CLIENT_NAMES: &[&str] = &[
    "diffPatch",
    "rawInputRendering",
    "sessionFailure",
    "recommendedValue",
    "backgroundSubagents",
    "nativeSubagentSessions",
    "loadReplayEnd",
    "planContentDelta",
    "asyncTasks",
    "customInstructions",
];

/// Feature for steering requests ("Send now" in JetBrains IDEs).
pub(super) const STEERING: &str = "steering";

/// Client request methods that have a prompt with content blocks.
pub(super) const PROMPT_METHODS: &[&str] = &["_session/steering"];

/// Get the agent feature that a JetBrains client request needs.
/// Returns:
///   The feature name, or `None` if the request needs no such feature.
pub(super) fn required(method: &str) -> Option<&'static str> {
    Some(match method {
        "_session/steering" => STEERING,
        "_session/goal" => GOAL,
        "_session/async_task/stop" => "air:asyncTasks",
        _ => return None,
    })
}

/// Client `_meta` features of JetBrains that the agent gets in restrict
/// mode.
// The feature only shows the output that the agent puts in its tool
// calls. The client runs no command for it.
const META_NAMES: &[&str] = &["terminal_output_delta"];

/// All JetBrains feature names that a set can have.
pub(super) fn known() -> impl Iterator<Item = String> {
    [STEERING, GOAL]
        .into_iter()
        .map(str::to_string)
        .chain(AGENT_NAMES.iter().map(|n| format!("{PREFIX}{n}")))
}

/// Copy the safe JetBrains features from the client `_meta` to the kept
/// `_meta`.
/// Args:
///  - `meta`: Client `_meta` features
///
/// Returns:
///   The kept `_meta` features.
pub(super) fn keep_meta(meta: &Map<String, Value>) -> Map<String, Value> {
    let mut kept = super::keep_names(meta, META_NAMES);
    if let Some(air) = meta
        .get("jetbrains")
        .and_then(|j| j.get("air"))
        .and_then(Value::as_object)
    {
        kept.insert("jetbrains".into(), json!({ "air": restrict_air(air) }));
    }
    kept
}

/// Keep only the integer version and the safe names of the JetBrains AIR
/// client features.
/// Returns:
///   The AIR features with a list of names. The list is empty if the
///   client gave no valid list.
fn restrict_air(air: &Map<String, Value>) -> Value {
    let names: Vec<&Value> = air
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|name| name.as_str().is_some_and(|n| AIR_CLIENT_NAMES.contains(&n)))
        .collect();
    let mut kept = json!({ "capabilities": names });
    if let Some(version) = air.get("version").filter(|v| v.is_i64() || v.is_u64()) {
        kept["version"] = version.clone();
    }
    kept
}

/// Add the JetBrains features of a set to the handshake reply.
/// Args:
///  - `result`: `initialize` result
///  - `names`: Feature names of the set. Names that are not JetBrains
///    features are ignored.
///  - `client`: Client `initialize` params after the filter
///
/// Returns:
///   The `initialize` result with the JetBrains features added.
// The real agents add the AIR features only for a client that has AIR. The
// proxy does the same.
pub(super) fn offer(mut result: Value, names: &[String], client: &Value) -> Value {
    let client_air = client
        .pointer("/clientCapabilities/_meta/jetbrains/air")
        .filter(|air| air.is_object());
    let mut air = Vec::new();
    let mut goal = false;
    for name in names {
        if name == STEERING {
            result["_meta"]["steering"] = json!({ "supported": true });
        } else if name == GOAL {
            goal = true;
        } else if let Some(air_name) = name.strip_prefix(PREFIX) {
            air.push(air_name);
        }
    }
    if client_air.is_none() || (air.is_empty() && !goal) {
        return result;
    }
    let mut value = json!({ "version": 1, "capabilities": air });
    if goal {
        value["goal"] = json!({
            "version": 1,
            "controlMethod": "_session/goal",
            "actions": ["set", "clear"],
        });
    }
    result["_meta"]["jetbrains"]["air"] = value;
    result
}

/// Test if a JetBrains feature is present in an `initialize` result.
/// Args:
///  - `init`: `initialize` result
///  - `name`: Feature name
///
/// Returns:
///   `None` if the name is not a JetBrains feature, else if it is present.
pub(super) fn present(init: &Value, name: &str) -> Option<bool> {
    if name == STEERING {
        return Some(super::has(init, "/_meta/steering/supported"));
    }
    if name == GOAL {
        return Some(super::has(init, "/_meta/jetbrains/air/goal"));
    }
    let air_name = name.strip_prefix(PREFIX)?;
    Some(
        init.pointer("/_meta/jetbrains/air/capabilities")
            .and_then(Value::as_array)
            .is_some_and(|caps| caps.iter().any(|c| c == air_name)),
    )
}

#[cfg(test)]
mod tests {
    //! Tests for the JetBrains client features.

    use super::super::prepare_initialize_params;
    use super::*;

    /// Test that a malformed JetBrains AIR part gives no AIR names to the
    /// agent. A client must not get unsafe names through with a value of
    /// an unexpected type.
    ///   1. Restrict an AIR part with a text version and a text list
    ///   2. Check that the AIR part stays with an empty list and no version
    ///   3. Restrict an AIR part that is not an object, and check that the
    ///      agent gets no `_meta`
    #[test]
    fn malformed_air_client_features_give_no_names() {
        let restrict = |air: Value| {
            let params =
                json!({ "clientCapabilities": { "_meta": { "jetbrains": { "air": air } } } });
            prepare_initialize_params(params, true)["clientCapabilities"].clone()
        };
        assert_eq!(
            restrict(json!({ "version": "1", "capabilities": "planFile" })),
            json!({ "_meta": { "jetbrains": { "air": { "capabilities": [] } } } })
        );
        assert_eq!(restrict(json!("planFile")), json!({}));
    }
}
