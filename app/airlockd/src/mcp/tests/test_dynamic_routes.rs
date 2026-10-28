//! Run-time MCP servers: the host adds a server at
//! `http://acp.mcp.airlock/<session>/<name>` and removes it with its grant.

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::oneshot;

use crate::mcp::{self, CONFIG_HOST, RUNTIME_HOST};
use crate::test_cfg::{
    FakeMcpHost, McpClient, add_runtime, block_on_local, eventually_async, http,
    read_until_contains, read_until_eof, serve_mcp, sse_events,
};

/// GET `/` of `host` and return the body. The status must be 200.
async fn list(addr: SocketAddr, host: &str) -> String {
    let header = format!("Host: {host}");
    let reply = http(addr, "GET", "/", &[&header], None).await;
    assert_eq!(reply.status, 200, "{reply:?}");
    reply.body
}

/// Test that a run-time server serves sessions until the host releases its
/// grant, and that the release returns after the sessions ended. The host
/// must know that no guest session uses the server after the release.
///   1. Add server `k1/echo` and check the route list
///   2. Start a session, send `tools/list`, open the standalone stream and
///      send a request that gets no answer
///   3. Release the grant
///   4. Check that the standalone stream ended, the open request got a
///      JSON-RPC error, the host process stops and the route is gone (HTTP
///      404 for the session and for a new `initialize`)
#[test]
fn runtime_server_serves_sessions_until_grant_release() {
    block_on_local(async {
        let (addr, routes) = serve_mcp(None).await;
        let host = Rc::new(FakeMcpHost::default());
        let grant = add_runtime(&routes, "k1", "echo", &host).await.unwrap();
        assert_eq!(list(addr, RUNTIME_HOST).await, "k1/echo\n");
        assert_eq!(list(addr, CONFIG_HOST).await, "");

        let mut client = McpClient::at(addr, RUNTIME_HOST, "/k1/echo");
        assert_eq!(client.initialize().await.status, 200);
        let tools = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let reply = client.post(&tools).await;
        assert_eq!(reply.events()[0]["result"]["tools"][0]["name"], "echo");
        let (mut stream, head) = client.listen().await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let mut hang = client
            .post_open(&json!({ "jsonrpc": "2.0", "id": 7, "method": "test/hang" }))
            .await;
        read_until_contains(&mut hang, "\r\n\r\n").await;
        assert_eq!(host.running.get(), 1);

        grant.release_request().send().promise.await.unwrap();
        read_until_eof(&mut stream).await;
        let text = read_until_eof(&mut hang).await;
        let error = text.lines().find(|l| l.starts_with("data: ")).unwrap();
        let error = &sse_events(error)[0];
        assert_eq!(error["id"], 7, "{text}");
        assert_eq!(error["error"]["message"], "the MCP server was removed");
        eventually_async("the host process stops", async || {
            (host.running.get() == 0).then_some(())
        })
        .await;
        assert_eq!(list(addr, RUNTIME_HOST).await, "");
        assert_eq!(client.post(&tools).await.status, 404);
        client.session = None;
        assert_eq!(client.initialize().await.status, 404);
        assert_eq!(host.opens.get(), 1);
    });
}

/// Test that the guest refuses a run-time server with a route that it
/// serves already, bad route parts and servers over the limit. One route
/// must have only one host, the parts must be safe in a URL path, and a
/// dropped grant must free its route.
///   1. Add server `k1/echo`
///   2. Check that a second `k1/echo` is refused and that `k2/echo` and
///      `k1/other` are added
///   3. Check that empty, long and non-URL-safe parts are refused
///   4. Check that the first server still serves its sessions
///   5. Release the first grant and check that `k1/echo` can be added again
///   6. Drop the `k2/echo` grant and check that its route is gone
///   7. Add servers up to the limit and check that one more is refused
#[test]
fn duplicate_session_and_name_is_refused() {
    block_on_local(async {
        let (addr, routes) = serve_mcp(None).await;
        let host = Rc::new(FakeMcpHost::default());
        let first = add_runtime(&routes, "k1", "echo", &host).await.unwrap();

        let other = Rc::new(FakeMcpHost::default());
        let err = add_runtime(&routes, "k1", "echo", &other).await.err();
        assert!(err.is_some_and(|e| e.to_string().contains("exists already")));
        // A local client: the drop of the last client drops the grant. A
        // loopback connection keeps its bootstrap capability.
        let k2 = mcp::add_server(&routes, "k2", "echo", other.client())
            .await
            .unwrap();
        let _named = add_runtime(&routes, "k1", "other", &other).await.unwrap();
        let long = "a".repeat(65);
        for (session, name) in [
            ("", "echo"),
            ("k1", ""),
            (long.as_str(), "echo"),
            ("k1", "a/b"),
        ] {
            let err = add_runtime(&routes, session, name, &other).await.err();
            assert!(err.is_some(), "{session:?} {name:?}");
        }
        let max = "a".repeat(64);
        let _max = add_runtime(&routes, &max, "A_b-9", &other).await.unwrap();

        let mut client = McpClient::at(addr, RUNTIME_HOST, "/k1/echo");
        assert_eq!(client.initialize().await.status, 200);
        assert_eq!((host.opens.get(), other.opens.get()), (1, 0));

        first.release_request().send().promise.await.unwrap();
        let _again = add_runtime(&routes, "k1", "echo", &other).await.unwrap();

        drop(k2);
        eventually_async("the dropped route is gone", async || {
            (!list(addr, RUNTIME_HOST).await.contains("k2/echo")).then_some(())
        })
        .await;

        // Three servers are left: `k1/echo`, `k1/other` and the long one.
        let mut grants = Vec::new();
        for i in 3..256 {
            grants.push(
                add_runtime(&routes, &format!("n{i}"), "echo", &other)
                    .await
                    .unwrap(),
            );
        }
        let err = add_runtime(&routes, "full", "echo", &other).await.err();
        assert!(err.is_some_and(|e| e.to_string().contains("too many")));
    });
}

/// Test that boot config servers and run-time servers use their own host
/// and their own host name. A guest client must not get to a server of the
/// other kind.
///   1. Serve config server `echo` of host A and run-time server `k1/echo`
///      of host B
///   2. Check the route list of each host name
///   3. Start a session on each and check that each host got one open
///   4. Check that the paths and the session IDs of one host name give
///      HTTP 404 on the other
#[test]
fn config_and_runtime_hosts_are_separate() {
    block_on_local(async {
        let config = Rc::new(FakeMcpHost::default());
        let (addr, routes) = serve_mcp(Some(&config)).await;
        let runtime = Rc::new(FakeMcpHost::default());
        let _grant = add_runtime(&routes, "k1", "echo", &runtime).await.unwrap();
        assert_eq!(list(addr, CONFIG_HOST).await, "echo\nbroken\n");
        assert_eq!(list(addr, RUNTIME_HOST).await, "k1/echo\n");

        let mut on_config = McpClient::new(addr);
        assert_eq!(on_config.initialize().await.status, 200);
        assert_eq!((config.opens.get(), runtime.opens.get()), (1, 0));
        let mut on_runtime = McpClient::at(addr, RUNTIME_HOST, "/k1/echo");
        assert_eq!(on_runtime.initialize().await.status, 200);
        assert_eq!((config.opens.get(), runtime.opens.get()), (1, 1));

        assert_eq!(
            McpClient::at(addr, RUNTIME_HOST, "/echo")
                .initialize()
                .await
                .status,
            404
        );
        assert_eq!(
            McpClient::at(addr, CONFIG_HOST, "/k1/echo")
                .initialize()
                .await
                .status,
            404
        );
        let tools = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        on_runtime.session = on_config.session.clone();
        assert_eq!(on_runtime.post(&tools).await.status, 404);
        assert_eq!((config.opens.get(), runtime.opens.get()), (1, 1));
    });
}

/// Test that a grant release during an open ends the new session and stops
/// its host process. The release must wait for the open, so no process is
/// left on the host after it returns.
///   1. Make the host wait in `open` and send `initialize`
///   2. Release the grant and check that the route is gone at once, but
///      the release waits
///   3. Let the host start the process
///   4. Check that the release returns, `initialize` gets HTTP 404 and the
///      host process stops
#[test]
fn open_in_flight_during_release_ends_cleanly() {
    block_on_local(async {
        let (addr, routes) = serve_mcp(None).await;
        let host = Rc::new(FakeMcpHost::default());
        let grant = add_runtime(&routes, "k1", "echo", &host).await.unwrap();
        let (open, gate) = oneshot::channel();
        *host.open_gate.borrow_mut() = Some(gate);

        let client = McpClient::at(addr, RUNTIME_HOST, "/k1/echo");
        let mut init = client
            .post_open(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" }))
            .await;
        eventually_async("the host starts the open", async || {
            (host.opens.get() == 1).then_some(())
        })
        .await;

        let released = Rc::new(Cell::new(false));
        let release = grant.release_request().send().promise;
        tokio::task::spawn_local({
            let released = released.clone();
            async move {
                release.await.unwrap();
                released.set(true);
            }
        });
        eventually_async("the route is gone", async || {
            list(addr, RUNTIME_HOST).await.is_empty().then_some(())
        })
        .await;
        // Give the release time to return if it does not wait.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!released.get(), "release returned before the open ended");

        open.send(()).unwrap();
        eventually_async("the release returns", async || released.get().then_some(())).await;
        let text = read_until_contains(&mut init, "\r\n\r\n").await;
        assert!(text.starts_with("HTTP/1.1 404"), "{text}");
        eventually_async("the process starts and stops", async || {
            (host.started.get() == 1 && host.running.get() == 0).then_some(())
        })
        .await;
    });
}
