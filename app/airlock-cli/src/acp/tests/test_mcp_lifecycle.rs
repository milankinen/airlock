//! Tests for the lifetime of the editor MCP servers of the ACP sessions.

use std::time::{Duration, Instant};

use airlock_common::supervisor_capnp::mcp_host;
use serde_json::{Value, json};

use crate::test_cfg::acp::{
    FakeRegistrar, LocalProcessLauncher, Registrations, TestClient, mcp_config, seen,
};
use crate::test_cfg::mcp_grant::Route;
use crate::test_cfg::{TempDir, block_on_local, mcp_session, temp_dir};

/// A stdio server entry with the name `name`. Its program reads its input
/// until the end.
fn server(name: &str) -> Value {
    json!({ "name": name, "command": "cat", "args": [], "env": [] })
}

/// Start a proxy with the MCP bridge and the fake registrar, and do the
/// client handshake.
/// Args:
///  - `env`: More environment variables of the agent
///
/// Returns:
///   The client, the test side of the registrar and the session dir. The
///   server programs run in it.
async fn bridged(env: &[(&str, &str)]) -> (TestClient, Registrations, TempDir) {
    let (registrar, registrations) = FakeRegistrar::start(&[]);
    let dir = temp_dir();
    let launcher = LocalProcessLauncher::new(env)
        .with_root(dir.path().to_str().unwrap())
        .with_registrar(registrar);
    let mut client = TestClient::start(launcher, mcp_config(true));
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
///   1. Start a session with two servers
///   2. Send `session/close` for the session
///   3. Check that both grants are released and the host opens no session
#[test]
fn session_close_releases_servers() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        let (response, _) = session(&mut client, &dir, 2, "session/new", "", &["a", "b"]).await;
        let sid = &response["result"]["sessionId"];
        let added = routes(&mut registrations, 2).await;
        assert!(opens(&added[0].host, &added[0].name).await);

        let (response, _) = client
            .request(3, "session/close", json!({ "sessionId": sid }))
            .await;
        assert!(response.get("result").is_some(), "{response}");
        registrations.released(2).await;
        assert!(!opens(&added[0].host, &added[0].name).await);
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
///   1. Start a session with server `a`
///   2. Fork the session with server `b`, and check that the fork runs in
///      its own agent with its own handshake
///   3. Check that `b` has a new key and `a` still opens sessions
///   4. Close the fork and check that only its server ends
#[test]
fn fork_keeps_source_servers() {
    block_on_local(async {
        let (mut client, mut registrations, dir) = bridged(&[]).await;
        let (response, _) = session(&mut client, &dir, 2, "session/new", "", &["a"]).await;
        let sid = response["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let source = registrations.route().await;
        let (response, others) = session(&mut client, &dir, 3, "session/fork", &sid, &["b"]).await;
        let fork_sid = response["result"]["sessionId"].clone();
        let inits = seen(&others)
            .iter()
            .filter(|m| m["method"] == "initialize")
            .count();
        assert_eq!(inits, 1, "{others:?}");
        assert_ne!(fork_sid, json!(sid));
        let fork = registrations.route().await;
        assert_ne!(fork.session, source.session);
        assert!(opens(&source.host, &source.name).await);

        client
            .request(4, "session/close", json!({ "sessionId": fork_sid }))
            .await;
        registrations.released(1).await;
        assert!(!opens(&fork.host, &fork.name).await);
        assert!(opens(&source.host, &source.name).await);
    });
}

/// Test that the agent exit ends the servers of its session, and only
/// them. Each session has its own agent.
///   1. Start sessions in sandboxes `a` and `b` with one server each
///   2. Make the agent of the first session exit
///   3. Check that its grant is released and the other server still opens
///      sessions
///   4. Close the client and check that the other grant is released
#[test]
fn agent_exit_releases_servers_of_its_session() {
    block_on_local(async {
        let (registrar, mut registrations) = FakeRegistrar::start(&[]);
        let (dir_a, dir_b) = (temp_dir(), temp_dir());
        let launcher = LocalProcessLauncher::new(&[])
            .with_root(dir_a.path().to_str().unwrap())
            .with_sandbox_root("b", dir_b.path().to_str().unwrap())
            .with_registrar(registrar);
        let mut client = TestClient::start(launcher, mcp_config(true));
        client.initialize(json!({})).await;
        let (first, _) = session(&mut client, &dir_a, 2, "session/new", "", &["a"]).await;
        session(&mut client, &dir_b, 3, "session/new", "", &["b"]).await;
        let added = routes(&mut registrations, 2).await;

        let params = json!({ "sessionId": first["result"]["sessionId"] });
        client.notify("test/exit", params).await;
        registrations.released(1).await;
        assert!(!opens(&added[0].host, &added[0].name).await);
        assert!(opens(&added[1].host, &added[1].name).await);

        assert_eq!(client.exit_code().await, 7);
        registrations.released(1).await;
        assert!(!opens(&added[1].host, &added[1].name).await);
    });
}

/// Test that a registration without a reply does not stop the proxy or
/// other sessions, and that the agent drops the server at the time limit.
/// A stuck sandbox must not stop the editor.
///   1. Use a registrar that never answers for server `stuck`, and a time
///      limit of 3 s
///   2. Send `session/new` with servers `ok` and `stuck`, and a second
///      `session/new` with server `ok2`
///   3. Check that the second session and the agent output came before the
///      time limit
///   4. Check that the first session got only `ok`, after the time limit
#[test]
fn stuck_registration_keeps_proxy_responsive_and_times_out() {
    let say = r#"{"jsonrpc":"2.0","method":"session/update","params":{"early":true}}"#;
    let limit = Duration::from_secs(3);
    block_on_local(async {
        let (registrar, mut registrations) = FakeRegistrar::start(&["stuck"]);
        let launcher = LocalProcessLauncher::new(&[("AGENT_SAY", say)]).with_registrar(registrar);
        let mut config = mcp_config(true);
        config.mcp_register_timeout = limit;
        let mut client = TestClient::start(launcher, config);
        client.initialize(json!({})).await;

        let servers = json!([server("ok"), server("stuck")]);
        let start = Instant::now();
        let params = json!({ "cwd": "/projects/a", "mcpServers": servers });
        client.send_request(2, "session/new", params).await;
        let params = json!({ "cwd": "/projects/a", "mcpServers": [server("ok2")] });
        client.send_request(3, "session/new", params).await;

        let (response, others) = client.response(3).await;
        assert!(start.elapsed() < limit);
        assert!(response.get("result").is_some(), "{response}");
        assert!(others.iter().any(|m| m["params"]["early"] == true));
        let (response, others) = client.response(2).await;
        assert!(start.elapsed() >= limit);
        assert!(response.get("result").is_some(), "{response}");

        let got = seen(&others);
        let request = got.iter().find(|m| m["id"] == 2).expect("session request");
        let names: Vec<&Value> = request["params"]["mcpServers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| &s["name"])
            .collect();
        assert_eq!(names, [&json!("ok")]);
        let mut names: Vec<String> = routes(&mut registrations, 2)
            .await
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        assert_eq!(names, ["ok", "ok2"]);
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
