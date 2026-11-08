//! Tests for the agent features in the handshake reply of the proxy.

use serde_json::{Value, json};

use crate::acp::capabilities;
use crate::test_cfg::acp::{LocalProcessLauncher, TestClient, test_config};
use crate::test_cfg::block_on_local;

/// Client features with a JetBrains AIR part.
/// Args:
///  - `names`: AIR names of the client
fn air_client(names: &[&str]) -> Value {
    json!({ "_meta": { "jetbrains": { "air": { "version": 1, "capabilities": names } } } })
}

/// Do the client handshake with the feature set `features`.
/// Returns:
///   The `initialize` result of the proxy.
async fn reply(features: Vec<String>, caps: Value) -> Value {
    let config = crate::acp::Config {
        agent_capabilities: features,
        ..test_config(true)
    };
    let mut client = TestClient::start(LocalProcessLauncher::new(&[]), config);
    client.initialize(caps).await
}

/// Test that the Claude preset offers the session, login and sub-agent
/// features, and the AIR features only to an AIR client. JetBrains IDEs
/// show these parts only when the agent offers them. The preset does not
/// offer the session index: its requests have no session id, and each
/// session has its own agent.
///   1. Do the handshake with the Claude preset and an AIR client with the
///      session index
///   2. Check the session features, the login and the AIR list with the
///      goal, and without the session index and its extras
///   3. Do it again with a client without AIR, and check that the reply
///      has no AIR part
#[test]
fn claude_preset_offers_air_features_only_to_air_client() {
    let claude = capabilities::preset(&["claude-agent-acp".into()]);
    block_on_local(async {
        let result = reply(claude.clone(), air_client(&["sessionIndex"])).await;
        let agent = &result["agentCapabilities"];
        assert_eq!(agent["loadSession"], true);
        assert_eq!(agent["sessionCapabilities"]["subagents"], json!({}));
        assert_eq!(agent["sessionCapabilities"]["fork"], json!({}));
        assert!(agent.get("auth").is_none());
        assert_eq!(agent["_meta"]["authStatus"], json!({}));
        assert_eq!(result["authMethods"], json!([]));
        assert_eq!(result["_meta"]["steering"]["supported"], true);
        let air = &result["_meta"]["jetbrains"]["air"];
        assert_eq!(air["goal"]["controlMethod"], "_session/goal");
        let names = air["capabilities"].as_array().unwrap();
        assert!(names.contains(&json!("backgroundSubagents")), "{air}");
        for name in ["sessionIndex", "sessionRename"] {
            assert!(!names.contains(&json!(name)), "{name}: {air}");
        }

        let result = reply(claude, json!({})).await;
        assert!(result["_meta"].get("jetbrains").is_none(), "{result}");
    });
}

/// Test that a feature set from `--set-agent-capabilities` replaces the
/// preset fully, also steering. The user can offer what the agent has when
/// the preset does not know the agent.
///   1. Do the handshake with only `sessionCapabilities.list`
///   2. Check that the reply has only the session list
///   3. Do the handshake with an empty set and check that the reply has no
///      features
#[test]
fn set_agent_capabilities_replaces_preset() {
    block_on_local(async {
        let names = capabilities::parse("sessionCapabilities.list").unwrap();
        let result = reply(names, air_client(&[])).await;
        assert_eq!(
            result["agentCapabilities"],
            json!({ "sessionCapabilities": { "list": {} } })
        );
        assert!(result.get("_meta").is_none(), "{result}");

        let result = reply(capabilities::parse("").unwrap(), json!({})).await;
        assert_eq!(result["agentCapabilities"], json!({}));
    });
}
