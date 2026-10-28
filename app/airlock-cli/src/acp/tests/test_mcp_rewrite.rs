//! Tests for the rewrite of the editor MCP servers in the session requests.

use serde_json::{Value, json};

use crate::test_cfg::acp::{
    FakeRegistrar, LocalStarter, Registrations, TestClient, mcp_config, mcp_route, seen,
    test_config,
};
use crate::test_cfg::block_on_local;

/// Start a proxy with the MCP bridge and the fake sandbox registrar, and
/// do the client handshake.
/// Returns:
///   The client, the test side of the registrar and the `initialize`
///   result of the proxy.
async fn bridged(restrict: bool, env: &[(&str, &str)]) -> (TestClient, Registrations, Value) {
    let (registrar, registrations) = FakeRegistrar::start(&[]);
    let starter = LocalStarter::new(env).with_registrar(registrar);
    let mut client = TestClient::start(starter, mcp_config(restrict));
    let init = client.initialize(json!({})).await;
    (client, registrations, init)
}

/// Send a session request with the MCP servers `servers`.
/// Returns:
///   The `mcpServers` list that the agent got.
async fn agent_servers(client: &mut TestClient, id: i64, method: &str, servers: Value) -> Value {
    let params = json!({ "sessionId": "s", "cwd": "/projects/a", "mcpServers": servers });
    let (response, others) = client.request(id, method, params).await;
    assert!(response.get("result").is_some(), "{response}");
    let got = seen(&others);
    let request = got.iter().find(|m| m["id"] == id).expect("agent request");
    request["params"]["mcpServers"].clone()
}

/// The keys and the names of the MCP addresses in `servers`.
fn routes(servers: &Value) -> Vec<(String, String)> {
    servers
        .as_array()
        .unwrap()
        .iter()
        .map(|s| mcp_route(s["url"].as_str().unwrap()).unwrap())
        .collect()
}

/// Test that without `--proxy-mcp-servers` the agent gets no editor MCP
/// servers in both modes. Editor commands and secrets must not go to the
/// sandbox unless the user allows it.
///   1. Start a proxy without the bridge in restrict and in pass mode
///   2. Check that the `initialize` result has no MCP features
///   3. Send `session/new` and `session/load` with editor MCP servers
///   4. Check that the agent got empty lists
#[test]
fn proxy_without_flag_gives_agent_no_mcp_servers() {
    block_on_local(async {
        for restrict in [true, false] {
            let mut client = TestClient::start(LocalStarter::new(&[]), test_config(restrict));
            let init = client.initialize(json!({})).await;
            assert!(init["agentCapabilities"].get("mcpCapabilities").is_none());
            let servers = json!([
                { "name": "local", "command": "tool", "args": [], "env": [] },
                { "type": "http", "name": "web", "url": "http://localhost:1", "headers": [] },
                { "type": "acp", "name": "editor", "id": "e1" },
            ]);
            for (id, method) in [(2, "session/new"), (3, "session/load")] {
                let got = agent_servers(&mut client, id, method, servers.clone()).await;
                assert_eq!(got, json!([]), "restrict={restrict} {method}");
            }
        }
    });
}

/// Test that in restrict mode the proxy bridges stdio, HTTP and SSE
/// servers, and drops the other entries. The agent must get only sandbox
/// addresses, with the original names and no headers.
///   1. Start a proxy with the bridge and check the MCP features of the
///      `initialize` result
///   2. Send `session/new` with stdio (with and without type), HTTP, SSE,
///      editor, unknown and not valid entries
///   3. Check that the agent got four HTTP entries in order, with one key
///   4. Check that the sandbox got the same routes
#[test]
fn restrict_mode_bridges_supported_servers_and_drops_others() {
    block_on_local(async {
        let (mut client, mut registrations, init) = bridged(true, &[]).await;
        assert_eq!(
            init["agentCapabilities"]["mcpCapabilities"],
            json!({ "http": true, "sse": true })
        );
        let secret = json!([{ "name": "authorization", "value": "Bearer secret" }]);
        let servers = json!([
            { "name": "one", "command": "tool", "args": ["-x"], "env": [] },
            { "type": "stdio", "name": "two", "command": "tool", "args": [], "env": [] },
            { "type": "http", "name": "web", "url": "http://localhost:1/mcp", "headers": secret },
            { "type": "sse", "name": "old", "url": "https://localhost:2/sse", "headers": [] },
            { "type": "acp", "name": "editor", "id": "e1" },
            { "type": "future", "name": "new" },
            { "name": "no-command", "args": [] },
            { "type": "http", "name": "bad", "url": "ftp://localhost" },
            { "type": "http", "name": "host", "url": "http://x", "headers": [
                { "name": "host", "value": "y" },
            ] },
        ]);
        let got = agent_servers(&mut client, 2, "session/new", servers).await;

        let got_routes = routes(&got);
        let key = &got_routes[0].0;
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
        for (i, name) in ["one", "two", "web", "old"].iter().enumerate() {
            assert_eq!(
                got[i],
                json!({
                    "type": "http",
                    "name": name,
                    "url": format!("http://acp.mcp.airlock/{key}/{name}"),
                    "headers": [],
                })
            );
        }
        assert_eq!(got.as_array().unwrap().len(), 4, "{got}");
        let mut added = Vec::new();
        for _ in 0..4 {
            let route = registrations.route().await;
            added.push((route.session, route.name));
        }
        added.sort();
        let mut want = got_routes.clone();
        want.sort();
        assert_eq!(added, want);
    });
}

/// Test that the proxy makes safe and unique names for the sandbox
/// addresses. The names go into URL paths, and the agent still sees the
/// original names.
///   1. Send servers with spaces, an empty name, non-ASCII characters, a
///      long name and the same names two times
///   2. Check the address names and the original names
#[test]
fn server_names_are_sanitized_and_unique() {
    block_on_local(async {
        let (mut client, _registrations, _) = bridged(true, &[]).await;
        let long = "a".repeat(100);
        let names = [
            "my server",
            "my server",
            "",
            "äö/x",
            &long,
            &long,
            "my_server",
        ];
        let servers: Vec<Value> = names
            .iter()
            .map(|n| json!({ "name": n, "command": "tool", "args": [], "env": [] }))
            .collect();
        let got = agent_servers(&mut client, 2, "session/new", json!(servers)).await;

        let got_names: Vec<String> = routes(&got).into_iter().map(|(_, n)| n).collect();
        let a64 = "a".repeat(64);
        let a62 = "a".repeat(62);
        let want = [
            "my_server".to_string(),
            "my_server-2".into(),
            "mcp".into(),
            "___x".into(),
            a64,
            format!("{a62}-2"),
            "my_server-3".into(),
        ];
        assert_eq!(got_names, want);
        let original: Vec<&str> = got
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(original, names);
    });
}

/// Test that two sessions with the same server get different keys. The
/// sessions must not share server sessions or addresses.
///   1. Send two `session/new` requests with the same server
///   2. Check that the addresses have the same name and different keys
#[test]
fn sessions_with_same_server_get_separate_keys() {
    block_on_local(async {
        let (mut client, _registrations, _) = bridged(true, &[]).await;
        let servers = json!([{ "name": "tool", "command": "tool", "args": [], "env": [] }]);
        let first = agent_servers(&mut client, 2, "session/new", servers.clone()).await;
        let second = agent_servers(&mut client, 3, "session/new", servers).await;
        let (first, second) = (&routes(&first)[0], &routes(&second)[0]);
        assert_eq!(first.1, second.1);
        assert_ne!(first.0, second.0);
    });
}

/// Test that an agent without HTTP MCP support gets no bridged servers,
/// and the session still starts. The agent cannot use the addresses.
///   1. Use an agent without MCP features
///   2. Send `session/new` with a stdio server
///   3. Check that the agent got an empty list and the sandbox got no
///      route
#[test]
fn agent_without_http_support_gets_no_bridged_servers() {
    let init = json!({ "protocolVersion": 1, "agentCapabilities": {} }).to_string();
    block_on_local(async {
        let (mut client, mut registrations, _) = bridged(true, &[("AGENT_INIT", &init)]).await;
        let servers = json!([{ "name": "tool", "command": "tool", "args": [], "env": [] }]);
        let got = agent_servers(&mut client, 2, "session/new", servers).await;
        assert_eq!(got, json!([]));
        assert!(registrations.routes_now().is_empty());
    });
}

/// Test that pass mode with the bridge bridges the supported servers and
/// keeps the other entries as they are. The user gave the agent all
/// editor features.
///   1. Start the proxy in pass mode with the bridge
///   2. Send `session/new` with a stdio, an editor and an unknown entry
///   3. Check that the stdio server became an address and the other
///      entries did not change
#[test]
fn pass_mode_bridges_supported_servers_and_keeps_others() {
    block_on_local(async {
        let (mut client, _registrations, _) = bridged(false, &[]).await;
        let editor = json!({ "type": "acp", "name": "editor", "id": "e1" });
        let unknown = json!({ "type": "future", "name": "new", "x": 1 });
        let servers = json!([
            editor,
            { "name": "tool", "command": "tool", "args": [], "env": [] },
            unknown,
        ]);
        let got = agent_servers(&mut client, 2, "session/new", servers).await;
        assert_eq!(got[0], editor);
        assert_eq!(got[2], unknown);
        assert_eq!(got[1]["name"], "tool");
        assert_eq!(
            mcp_route(got[1]["url"].as_str().unwrap()).unwrap().1,
            "tool"
        );
    });
}
