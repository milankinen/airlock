//! Tests for an editor remote MCP server that the host relays.

use axum::http::Method;
use serde_json::json;

use crate::test_cfg::acp::{FakeRegistrar, LocalStarter, TestClient, mcp_config, seen};
use crate::test_cfg::mcp_remote::{SESSION_ID, streamable_server};
use crate::test_cfg::{block_on_local, mcp_session};

/// Test that the host relays an editor HTTP server with the editor
/// headers, and ends the remote session when the editor closes. The
/// headers must stay on the host, and the server must learn the end.
///   1. Start a fake Streamable HTTP server
///   2. Start a session with the server and an authorization header
///   3. Check that the agent got a sandbox address without headers
///   4. Open a server session on the host object that the sandbox got and
///      send `initialize`
///   5. Check the answer and the header at the server
///   6. Close the client and check that the server got a DELETE for the
///      session
#[test]
fn http_server_is_relayed_and_deleted_at_client_close() {
    block_on_local(async {
        let (fake, url) = streamable_server().await;
        let (registrar, mut registrations) = FakeRegistrar::start(&[]);
        let starter = LocalStarter::new(&[]).with_registrar(registrar);
        let mut client = TestClient::start(starter, mcp_config(true));
        client.initialize(json!({})).await;
        let server = json!({
            "type": "http",
            "name": "web",
            "url": url,
            "headers": [{ "name": "authorization", "value": "Bearer secret" }],
        });
        let params = json!({ "cwd": "/projects/a", "mcpServers": [server] });
        let (_, others) = client.request(2, "session/new", params).await;
        let got = seen(&others);
        let entry = &got[1]["params"]["mcpServers"][0];
        assert_eq!(entry["headers"], json!([]));
        assert!(!entry.to_string().contains("secret"), "{entry}");

        let route = registrations.route().await;
        let (session, mut events) = mcp_session::open(&route.host, &route.name, false)
            .await
            .unwrap();
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
        mcp_session::send(&session, &init.to_string())
            .await
            .unwrap();
        let answer = mcp_session::next_json(&mut events).await;
        assert!(answer["result"]["protocolVersion"].is_string(), "{answer}");
        let post = &fake.requests()[0];
        assert_eq!(post.header("authorization"), Some("Bearer secret"));

        assert_eq!(client.exit_code().await, 0);
        let deleted = fake
            .requests()
            .iter()
            .any(|r| r.method == Method::DELETE && r.header("mcp-session-id") == Some(SESSION_ID));
        assert!(deleted, "{:?}", fake.requests());
    });
}
