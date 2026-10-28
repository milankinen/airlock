//! Tests for the lifetime of the editor MCP servers of the ACP sessions.

use std::time::{Duration, Instant};

use airlock_common::supervisor_capnp::mcp_host;
use serde_json::{Value, json};

use crate::test_cfg::acp::{
    FakeRegistrar, LocalStarter, Registrations, TestClient, mcp_config, mcp_route, seen,
};
use crate::test_cfg::mcp_grant::Route;
use crate::test_cfg::{TempDir, block_on_local, mcp_session, temp_dir};

/// A stdio server entry with the name `name`. Its program reads its input
/// until the end.
fn server(name: &str) -> Value {
    json!({ "name": name, "command": "cat", "args": [], "env": [] })
}

/// Start a proxy with the MCP bridge, the fake registrar and an agent that
/// gives session ids, and do the client handshake.
/// Args:
///  - `env`: More environment variables of the agent
///
/// Returns:
///   The client, the test side of the registrar and the session dir. The
///   server programs run in it.
async fn bridged(env: &[(&str, &str)]) -> (TestClient, Registrations, TempDir) {
    let (registrar, registrations) = FakeRegistrar::start(&[]);
    let mut env = env.to_vec();
    env.push(("AGENT_SESSION_IDS", "1"));
    let dir = temp_dir();
    let starter = LocalStarter::new(&env)
        .with_root(dir.path().to_str().unwrap())
        .with_registrar(registrar);
    let mut client = TestClient::start(starter, mcp_config(true));
    client.initialize(json!({})).await;
    (client, registrations, dir)
}

/// Send a session request and get its response.
/// Returns:
///   The response, and the messages that came before it.
async fn session(
    client: &mut TestClient,
    dir: &TempDir,
    id: i64,
    method: &str,
    session: &str,
    servers: &[&str],
) -> (Value, Vec<Value>) {
    let servers: Vec<Value> = servers.iter().map(|n| server(n)).collect();
    let cwd = dir.path().to_str().unwrap();
    let params = json!({ "sessionId": session, "cwd": cwd, "mcpServers": servers });
    client.request(id, method, params).await
}

/// Wait for `count` routes in the sandbox.
async fn routes(registrations: &mut Registrations, count: usize) -> Vec<Route> {
    let mut routes = Vec::new();
    for _ in 0..count {
        routes.push(registrations.route().await);
    }
    routes
}

/// Check if the host of `route` still opens server sessions.
async fn opens(host: &mcp_host::Client, name: &str) -> bool {
    mcp_session::open(host, name, false).await.is_ok()
}

/// Test that `session/close` ends the servers of the session. The sandbox
/// must not keep the addresses of a closed session.
///   1. Start session `s1` with two servers
///   2. Send `session/close` for `s1`
///   3. Check that both grants are released and the host opens no session
#[test]
fn session_close_releases_servers() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        let (response, _) = session(&mut client, &dir, 2, "session/new", "", &["a", "b"]).await;
        assert_eq!(response["result"]["sessionId"], "s1");
        let added = routes(&mut registrations, 2).await;
        assert!(opens(&added[0].host, &added[0].name).await);

        let (response, _) = client
            .request(3, "session/close", json!({ "sessionId": "s1" }))
            .await;
        assert!(response.get("result").is_some(), "{response}");
        registrations.released(2).await;
        assert!(!opens(&added[0].host, &added[0].name).await);
    });
}

/// Test that `session/load` of a known session keeps its key and replaces
/// its servers. The URLs of the session stay the same, and the old servers
/// end first.
///   1. Start session `s1` with server `a`
///   2. Load `s1` with server `b`
///   3. Check that the old grant is released and its host is closed
///   4. Check that `b` has the key of `s1`
#[test]
fn load_of_known_session_keeps_key_and_replaces_servers() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        session(&mut client, &dir, 2, "session/new", "", &["a"]).await;
        let old = registrations.route().await;

        let (response, others) = session(&mut client, &dir, 3, "session/load", "s1", &["b"]).await;
        assert!(response.get("result").is_some(), "{response}");
        registrations.released(1).await;
        assert!(!opens(&old.host, &old.name).await);
        let new = registrations.route().await;
        assert_eq!(
            (new.session.as_str(), new.name.as_str()),
            (old.session.as_str(), "b")
        );
        let got = seen(&others);
        let url = got[0]["params"]["mcpServers"][0]["url"].as_str().unwrap();
        assert_eq!(mcp_route(url).unwrap(), (old.session, "b".into()));
    });
}

/// Test that a failed session request ends its servers. The agent has no
/// session that can use them.
///   1. Use an agent that replies to each request with an error
///   2. Send `session/new` with one server
///   3. Check that the grant is released
#[test]
fn failed_session_request_releases_servers() {
    let error = r#"{"code":-32603,"message":"no"}"#;
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[("AGENT_ERROR", error)]).await;
        let (response, _) = session(&mut client, &dir, 2, "session/new", "", &["a"]).await;
        assert_eq!(response["error"]["code"], -32603);
        let route = registrations.route().await;
        registrations.released(1).await;
        assert!(!opens(&route.host, &route.name).await);
    });
}

/// Test that `session/fork` gives the new session its own key and keeps
/// the servers of the source session.
///   1. Start session `s1` with server `a`
///   2. Fork `s1` with server `b`
///   3. Check that `b` has a new key and `a` still opens sessions
///   4. Close the fork and check that only its server ends
#[test]
fn fork_keeps_source_servers() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        session(&mut client, &dir, 2, "session/new", "", &["a"]).await;
        let source = registrations.route().await;
        let (response, _) = session(&mut client, &dir, 3, "session/fork", "s1", &["b"]).await;
        assert_eq!(response["result"]["sessionId"], "s2");
        let fork = registrations.route().await;
        assert_ne!(fork.session, source.session);
        assert!(opens(&source.host, &source.name).await);

        client
            .request(4, "session/close", json!({ "sessionId": "s2" }))
            .await;
        registrations.released(1).await;
        assert!(!opens(&fork.host, &fork.name).await);
        assert!(opens(&source.host, &source.name).await);
    });
}

/// Test that the agent exit ends the servers of all sessions. The proxy
/// stops with the agent.
///   1. Start two sessions with one server each
///   2. Make the agent exit
///   3. Check that the proxy ends and both grants are released
#[test]
fn agent_exit_releases_all_servers() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        session(&mut client, &dir, 2, "session/new", "", &["a"]).await;
        session(&mut client, &dir, 3, "session/new", "", &["b"]).await;
        let added = routes(&mut registrations, 2).await;

        client
            .send(&json!({ "jsonrpc": "2.0", "method": "test/exit" }))
            .await;
        assert_eq!(client.proxy_end().await, 7);
        registrations.released(2).await;
        for route in &added {
            assert!(!opens(&route.host, &route.name).await);
        }
    });
}

/// Test that a registration without a reply does not stop the proxy, and
/// that the proxy drops the server at the time limit. A stuck sandbox must
/// not stop the editor session. The client lines keep their order.
///   1. Use a registrar that never answers for server `stuck`, and a time
///      limit of 3 s
///   2. Send `session/new` with servers `ok` and `stuck`, a prompt, and a
///      second `session/new`
///   3. Check that the agent output came before the time limit
///   4. Check that the first session got only `ok`, after the time limit
///   5. Check that the agent got the three requests in order
#[test]
fn stuck_registration_keeps_proxy_responsive_and_times_out() {
    let say = r#"{"jsonrpc":"2.0","method":"session/update","params":{"early":true}}"#;
    let limit = Duration::from_secs(3);
    block_on_local(async {
        let (registrar, mut registrations) = FakeRegistrar::start(&["stuck"]);
        let env = [("AGENT_SESSION_IDS", "1"), ("AGENT_SAY", say)];
        let starter = LocalStarter::new(&env).with_registrar(registrar);
        let mut config = mcp_config(true);
        config.mcp_register_timeout = limit;
        let mut client = TestClient::start(starter, config);
        client.initialize(json!({})).await;

        let request = |id: i64, method: &str, params: Value| json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let servers = json!([server("ok"), server("stuck")]);
        let start = Instant::now();
        let params = json!({ "cwd": "/projects/a", "mcpServers": servers });
        client.send(&request(2, "session/new", params)).await;
        let params = json!({ "sessionId": "s1", "prompt": [] });
        client.send(&request(3, "session/prompt", params)).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [server("ok2")] });
        client.send(&request(4, "session/new", params)).await;

        let early = client.recv().await;
        assert_eq!(early["params"]["early"], true, "{early}");
        assert!(start.elapsed() < limit);
        let (response, mut others) = client.response(2).await;
        assert!(start.elapsed() >= limit);
        assert_eq!(response["result"]["sessionId"], "s1");
        others.extend(client.response(3).await.1);
        others.extend(client.response(4).await.1);

        let got = seen(&others);
        let ids: Vec<&Value> = got
            .iter()
            .map(|m| &m["id"])
            .filter(|id| id.is_i64())
            .collect();
        assert_eq!(ids, [&json!(2), &json!(3), &json!(4)]);
        let names: Vec<&Value> = got[1]["params"]["mcpServers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| &s["name"])
            .collect();
        assert_eq!(names, [&json!("ok")]);
        assert_eq!(registrations.route().await.name, "ok");
        assert_eq!(registrations.route().await.name, "ok2");
    });
}

/// Test that `session/load` of an unknown session gets a new key, and the
/// proxy maps the session at the OK response. A later close must find the
/// servers.
///   1. Load the unknown session `x` with server `a`
///   2. Close `x` and check that the grant is released
#[test]
fn load_of_unknown_session_gets_new_key_and_close_releases_it() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        let (response, _) = session(&mut client, &dir, 2, "session/load", "x", &["a"]).await;
        assert!(response.get("result").is_some(), "{response}");
        let route = registrations.route().await;
        assert_eq!(route.session.len(), 16);

        client
            .request(3, "session/close", json!({ "sessionId": "x" }))
            .await;
        registrations.released(1).await;
        assert!(!opens(&route.host, &route.name).await);
    });
}
