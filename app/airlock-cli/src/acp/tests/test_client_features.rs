//! Tests for the client features that the agent gets.

use serde_json::{Value, json};

use crate::test_cfg::acp::{LocalStarter, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Client features of a JetBrains IDE, with host features, a hostile AIR
/// part and unknown keys.
fn idea_caps() -> Value {
    json!({
        "fs": { "readTextFile": true, "writeTextFile": true },
        "terminal": true,
        "auth": { "terminal": true, "_meta": { "gateway": true } },
        "elicitation": { "form": {}, "url": {} },
        "session": { "configOptions": {}, "notices": {} },
        "subagents": {},
        "plan": {},
        "_meta": {
            "terminal_output": true,
            "terminal_output_delta": true,
            "subagent-transcript": true,
            "terminal-auth": true,
            "github.com/copilot": {},
            "jetbrains": {
                "air": {
                    "version": 1,
                    "capabilities": [
                        "diffPatch", "planFile", "agentFileChangeReport",
                        "sessionIndex", 5, "futureName",
                    ],
                    "planDir": "/host/plans",
                },
                "other": {},
            },
        },
        "future": true,
    })
}

/// Start a session with the client features `caps`.
/// Returns:
///   The client features that the agent got.
async fn agent_caps(restrict: bool, caps: Value) -> Value {
    let mut client = TestClient::start(LocalStarter::new(&[]), test_config(restrict));
    client.initialize(caps).await;
    let params = json!({ "cwd": "/projects/a", "mcpServers": [] });
    let (_, others) = client.request(2, "session/new", params).await;
    seen(&others)[0]["params"]["clientCapabilities"].clone()
}

/// Test that the agent gets only the safe client features of a JetBrains
/// IDE in the default mode, and all features in the pass mode. The IDE
/// shows sub-agents and terminal output only when the agent knows these
/// features. The AIR plan file and file change report make the IDE act on
/// host paths.
///   1. Start a session with the IDE features in the default mode
///   2. Check that the agent got the form prompts, the session, sub-agent
///      and plan features, the terminal output display, the sub-agent
///      transcript and the safe AIR names
///   3. Start a session with the same features in the pass mode
///   4. Check that the agent got all features
#[test]
fn ide_client_features_reach_agent_only_when_safe() {
    block_on_local(async {
        assert_eq!(
            agent_caps(true, idea_caps()).await,
            json!({
                "elicitation": { "form": {} },
                "session": { "configOptions": {}, "notices": {} },
                "subagents": {},
                "plan": {},
                "_meta": {
                    "terminal_output": true,
                    "terminal_output_delta": true,
                    "subagent-transcript": true,
                    "jetbrains": {
                        "air": { "version": 1, "capabilities": ["diffPatch", "sessionIndex"] },
                    },
                },
            })
        );
        assert_eq!(agent_caps(false, idea_caps()).await, idea_caps());
    });
}
