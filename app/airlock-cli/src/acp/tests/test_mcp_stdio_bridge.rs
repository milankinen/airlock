//! Tests for an editor stdio MCP server that runs on the host.

use serde_json::json;

use crate::test_cfg::acp::{FakeRegistrar, LocalStarter, TestClient, mcp_config};
use crate::test_cfg::mcp_session::{self, Event};
use crate::test_cfg::{block_on_local, temp_dir};

/// MCP server that answers each line with its working dir and the value
/// of `$BRIDGE_VALUE`.
const SERVER: &str = r#"
while IFS= read -r line; do
  printf '{"jsonrpc":"2.0","id":1,"result":{"cwd":"%s","value":"%s"}}\n' "$(pwd)" "$BRIDGE_VALUE"
done
"#;

/// Test that an editor stdio server runs on the host in the session dir
/// with the editor env, and stops when the editor closes. The host
/// commands of the editor must not run in the sandbox, and must not live
/// after the proxy.
///   1. Start a session in a host dir with a shell server and an env value
///   2. Open a server session on the host object that the sandbox got
///   3. Send a message and check the dir and the env value in the answer
///   4. Close the client and check that the server session closes
#[test]
fn stdio_server_runs_on_host_in_session_dir() {
    block_on_local(async {
        let dir = temp_dir();
        // The server prints the resolved dir. On macOS the temp dir is
        // under a symlink.
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let root = root.to_str().unwrap().to_string();
        let (registrar, mut registrations) = FakeRegistrar::start(&[]);
        let starter = LocalStarter::new(&[])
            .with_root(&root)
            .with_registrar(registrar);
        let mut client = TestClient::start(starter, mcp_config(true));
        client.initialize(json!({})).await;
        let server = json!({
            "name": "local",
            "command": "sh",
            "args": ["-c", SERVER],
            "env": [{ "name": "BRIDGE_VALUE", "value": "from-editor" }],
        });
        let params = json!({ "cwd": root, "mcpServers": [server] });
        let (response, _) = client.request(2, "session/new", params).await;
        assert!(response.get("result").is_some(), "{response}");

        let route = registrations.route().await;
        let (session, mut events) = mcp_session::open(&route.host, &route.name, false)
            .await
            .unwrap();
        mcp_session::send(&session, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .await
            .unwrap();
        let answer = mcp_session::next_json(&mut events).await;
        assert_eq!(answer["result"]["cwd"], root.as_str());
        assert_eq!(answer["result"]["value"], "from-editor");

        assert_eq!(client.exit_code().await, 0);
        assert!(matches!(
            mcp_session::next(&mut events).await,
            Event::Closed(_)
        ));
    });
}
