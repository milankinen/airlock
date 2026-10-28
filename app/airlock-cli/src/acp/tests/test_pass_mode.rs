//! Tests for the proxy mode that gives the agent all client features.

use serde_json::json;

use crate::test_cfg::acp::{LocalStarter, TestClient, seen, test_config};
use crate::test_cfg::block_on_local;

/// Test that with `--pass-client-capabilities` the agent gets all client
/// features and the client gets all agent messages. The user can allow
/// host access for a trusted agent.
///   1. Use an agent that sends a file request and a batch after the
///      handshake
///   2. Do the handshake with file and terminal features and an editor
///      MCP server
///   3. Check that the agent got the features in version 1 params, and
///      the editor MCP server
///   4. Check that the client got the file request and the batch
#[test]
fn pass_mode_keeps_client_features_and_agent_requests() {
    let say = [
        r#"{"jsonrpc":"2.0","id":"r1","method":"fs/read_text_file","params":{}}"#,
        r#"[{"jsonrpc":"2.0","method":"_x/batch"}]"#,
    ]
    .join("\n");
    block_on_local(async {
        let starter = LocalStarter::new(&[("AGENT_SAY", &say)]);
        let mut client = TestClient::start(starter, test_config(false));

        let caps = json!({ "fs": { "readTextFile": true }, "terminal": true });
        let params =
            json!({ "protocolVersion": 2, "capabilities": {}, "clientCapabilities": caps });
        client.request(0, "initialize", params).await;
        let servers = json!([{ "type": "acp", "name": "editor", "id": "e1" }]);
        let params = json!({ "cwd": "/projects/a", "mcpServers": servers });
        let (_, others) = client.request(2, "session/new", params).await;

        assert_eq!(others[0]["method"], "fs/read_text_file");
        assert_eq!(
            others[1],
            json!([{ "jsonrpc": "2.0", "method": "_x/batch" }])
        );
        let seen = seen(&others);
        assert_eq!(
            seen[0]["params"],
            json!({ "protocolVersion": 1, "clientCapabilities": caps })
        );
        assert_eq!(seen[1]["params"]["mcpServers"], servers);
        assert_eq!(seen[1]["params"]["cwd"], "/work");
    });
}
