//! MCP Streamable HTTP flow: a client in the VM uses a host MCP server
//! through `http://mcp.airlock/<name>`.

use std::rc::Rc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::oneshot;

use crate::test_cfg::{
    FakeMcpHost, McpClient, block_on_local, eventually_async, http, read_until_contains,
    read_until_eof, serve_local, sse_events,
};

/// Test that `initialize` starts a session and that requests in the session
/// get their responses as event streams. MCP clients need the session ID
/// for all later requests.
///   1. Send `initialize` with no session ID
///   2. Check the event stream response and the 128-bit hex session ID
///   3. Send the `initialized` notification and check HTTP 202
///   4. Send `tools/list` and check the result
///   5. Send `initialize` again in the session and check HTTP 400
#[test]
fn initialize_starts_session_and_requests_get_event_streams() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let addr = serve_local(Some(&host)).await;
        let mut client = McpClient::new(addr);

        let reply = client.initialize().await;
        assert_eq!(reply.status, 200, "{reply:?}");
        assert_eq!(
            reply.header("content-type").as_deref(),
            Some("text/event-stream")
        );
        let sid = client.session.clone().unwrap();
        assert_eq!(sid.len(), 32);
        assert!(sid.chars().all(|c| c.is_ascii_hexdigit()), "{sid}");
        assert_eq!(reply.events()[0]["result"]["serverInfo"]["name"], "echo");

        let reply = client
            .post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        assert_eq!(reply.status, 202, "{reply:?}");
        assert_eq!(reply.body, "");

        let reply = client
            .post(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .await;
        assert_eq!(reply.status, 200, "{reply:?}");
        assert_eq!(
            reply.events(),
            vec![json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [{ "name": "echo" }] } })]
        );
        assert_eq!(host.running.get(), 1);

        assert_eq!(client.initialize().await.status, 400);
        assert_eq!(host.running.get(), 1);
    });
}

/// Test that the event stream of a request also carries the server messages
/// that arrive before the response. The client must see progress and server
/// requests, and it must answer the server requests.
///   1. Send `tools/call` to the server, which sends a notification and a
///      server request before the result
///   2. Check that the stream has the three messages in order and then ends
///   3. POST the answer to the server request, check HTTP 202 and check
///      that the server gets it
#[test]
fn request_stream_carries_server_messages_before_response() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let mut client = McpClient::new(serve_local(Some(&host)).await);
        client.initialize().await;

        let reply = client
            .post(
                &json!({ "jsonrpc": "2.0", "id": "call-1", "method": "tools/call",
                           "params": { "name": "echo", "arguments": { "text": "hi" } } }),
            )
            .await;
        let events = reply.events();
        assert_eq!(events.len(), 3, "{reply:?}");
        assert_eq!(events[0]["method"], "notifications/progress");
        assert_eq!(events[1]["method"], "roots/list");
        assert_eq!(events[2]["id"], "call-1");
        assert_eq!(events[2]["result"]["content"][0]["text"], "hi");

        let answer = json!({ "jsonrpc": "2.0", "id": "srv-1", "result": { "roots": [] } });
        assert_eq!(client.post(&answer).await.status, 202);
        eventually_async("the server gets the answer", async || {
            host.received.borrow().contains(&answer).then_some(())
        })
        .await;
    });
}

/// Test that server messages with no open request go to the standalone
/// event stream, and that a session has only one such stream. Servers send
/// log and list-change notifications at any time.
///   1. Open the standalone stream
///   2. Check that a second standalone stream gets HTTP 409
///   3. Make the server send a notification with no open request
///   4. Check that the notification arrives on the standalone stream
#[test]
fn unsolicited_server_message_goes_to_standalone_stream() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let mut client = McpClient::new(serve_local(Some(&host)).await);
        client.initialize().await;

        let (mut stream, head) = client.listen().await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(client.listen_refused().await.status, 409);

        let emit = json!({ "jsonrpc": "2.0", "method": "test/emit" });
        assert_eq!(client.post(&emit).await.status, 202);
        let text = read_until_contains(&mut stream, "unsolicited").await;
        let data = text.lines().find(|l| l.starts_with("data: ")).unwrap();
        assert_eq!(
            sse_events(data)[0]["method"],
            "notifications/message",
            "{text}"
        );
    });
}

/// Test that DELETE ends the session and stops the server process. Later
/// requests with the old ID or an unknown ID get HTTP 404, as the spec
/// says. An open request must get an error, so the client does not wait.
///   1. Start a session, open its standalone stream and send a request
///      that gets no answer
///   2. Send DELETE and check HTTP 200
///   3. Check that the host process stops, the standalone stream ends and
///      the request stream ends with a JSON-RPC error
///   4. Check that a request, a second DELETE and an unknown ID get 404
#[test]
fn deleted_session_stops_server_and_is_unknown() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let mut client = McpClient::new(serve_local(Some(&host)).await);
        client.initialize().await;
        let (mut stream, _) = client.listen().await;
        let mut hang = client
            .post_open(&json!({ "jsonrpc": "2.0", "id": 7, "method": "test/hang" }))
            .await;
        read_until_contains(&mut hang, "\r\n\r\n").await;

        assert_eq!(client.delete().await.status, 200);
        eventually_async("the server process stops", async || {
            (host.running.get() == 0).then_some(())
        })
        .await;
        read_until_eof(&mut stream).await;
        let text = read_until_eof(&mut hang).await;
        let error = text.lines().find(|l| l.starts_with("data: ")).unwrap();
        let error = &sse_events(error)[0];
        assert_eq!(error["id"], 7, "{text}");
        assert_eq!(error["error"]["message"], "the MCP session was deleted");

        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        assert_eq!(client.post(&list).await.status, 404);
        assert_eq!(client.delete().await.status, 404);
        client.session = Some("00000000000000000000000000000000".into());
        assert_eq!(client.post(&list).await.status, 404);
    });
}

/// Test that a server process that stops ends its session. The client must
/// get HTTP 404 and start a new session.
///   1. Send a request that makes the server stop with no answer
///   2. Check that the event stream ends with a JSON-RPC error for the
///      request
///   3. Check that the next request gets HTTP 404
#[test]
fn stopped_server_process_ends_session() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let mut client = McpClient::new(serve_local(Some(&host)).await);
        client.initialize().await;

        let reply = client
            .post(&json!({ "jsonrpc": "2.0", "id": 9, "method": "test/exit" }))
            .await;
        assert_eq!(reply.status, 200);
        let events = reply.events();
        assert_eq!(events.len(), 1, "{reply:?}");
        assert_eq!(events[0]["id"], 9);
        assert_eq!(events[0]["error"]["message"], "the MCP server stopped");

        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        assert_eq!(client.post(&list).await.status, 404);
    });
}

/// Test that requests that break the transport rules are refused before
/// they get to the server. Web pages of other origins must not get access
/// (DNS rebinding).
///   1. Send requests with a bad Accept header, a batch, no session ID, an
///      unknown server name and a foreign Origin
///   2. Check the error status of each
///   3. Check that a local Origin is allowed
///   4. Check that the server got only the `initialize` requests
#[test]
fn requests_that_break_transport_rules_are_refused() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let addr = serve_local(Some(&host)).await;
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" });
        let post = async |path: &str, headers: &[&str], body| {
            let mut all = vec!["Host: mcp.airlock"];
            all.extend_from_slice(headers);
            http(addr, "POST", path, &all, Some(body)).await.status
        };
        let both = "Accept: application/json, text/event-stream";

        assert_eq!(
            post("/echo", &["Accept: application/json"], &init).await,
            406
        );
        let batch = json!([init]);
        assert_eq!(post("/echo", &[both], &batch).await, 400);
        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        assert_eq!(post("/echo", &[both], &list).await, 400);
        assert_eq!(post("/other", &[both], &init).await, 404);
        let evil = "Origin: http://evil.example";
        assert_eq!(post("/echo", &[both, evil], &init).await, 403);
        let rebound = "Origin: http://mcp.airlock.evil.example";
        assert_eq!(post("/echo", &[both, rebound], &init).await, 403);

        let local = "Origin: http://localhost:3000";
        assert_eq!(post("/echo", &[both, local], &init).await, 200);
        assert_eq!(host.received.borrow().len(), 1);
        assert_eq!(host.received_with("initialize").len(), 1);
    });
}

/// Test that the local server picks the routes by the host name. Old admin
/// clients that use `localhost` must keep the admin routes, and without a
/// grant `mcp.airlock` has no servers.
///   1. GET `/` with the host names `admin.airlock`, `localhost`,
///      `MCP.AIRLOCK:80` and `mcp.airlock`
///   2. Check that only the `mcp.airlock` names get the server list
///   3. Start a server with no MCP grant and check that `mcp.airlock` gets
///      an empty list and `admin.airlock` still works
#[test]
fn local_server_picks_routes_by_host_name() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let addr = serve_local(Some(&host)).await;
        let get = async |addr, host: &str| {
            let header = format!("Host: {host}");
            http(addr, "GET", "/", &[&header], None).await
        };

        for name in ["admin.airlock", "localhost", "127.0.0.1:80"] {
            let reply = get(addr, name).await;
            assert_eq!(reply.status, 200);
            assert!(reply.body.contains("|_|"), "{name}: {reply:?}");
        }
        for name in ["mcp.airlock", "MCP.AIRLOCK:80"] {
            let reply = get(addr, name).await;
            assert_eq!(reply.status, 200);
            assert_eq!(reply.body, "echo\nbroken\n", "{name}");
        }

        let addr = serve_local(None).await;
        let reply = get(addr, "mcp.airlock").await;
        assert_eq!((reply.status, reply.body.as_str()), (200, ""));
        assert_eq!(get(addr, "admin.airlock").await.status, 200);
    });
}

/// Test that a new session ends when the client goes away before it gets
/// the session ID. Nobody can use or end such a session, so it must not
/// keep a server process on the host.
///   1. Make the host wait in `open`
///   2. Send `initialize` and close the connection during the wait
///   3. Let the host start the process
///   4. Check that the process stops
#[test]
fn client_gone_during_initialize_ends_new_session() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let client = McpClient::new(serve_local(Some(&host)).await);
        let (release, gate) = oneshot::channel();
        *host.open_gate.borrow_mut() = Some(gate);

        let stream = client
            .post_open(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" }))
            .await;
        eventually_async("the host starts the open", async || {
            (host.opens.get() == 1).then_some(())
        })
        .await;
        drop(stream);
        // Give the server time to see the closed connection.
        tokio::time::sleep(Duration::from_millis(100)).await;
        release.send(()).unwrap();

        eventually_async("the process starts and stops", async || {
            (host.started.get() == 1 && host.running.get() == 0).then_some(())
        })
        .await;
    });
}

/// Test that a client can send a request ID again after the stream of the
/// first request broke. Clients retry with the same ID after a network
/// error.
///   1. Send a request that gets no answer and wait for the stream to open
///   2. Check that the same ID gets HTTP 400 while the stream is open
///   3. Close the connection of the first request
///   4. Check that the same ID then gets its response
#[test]
fn request_id_is_free_again_after_client_went_away() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let mut client = McpClient::new(serve_local(Some(&host)).await);
        client.initialize().await;

        let mut hang = client
            .post_open(&json!({ "jsonrpc": "2.0", "id": 5, "method": "test/hang" }))
            .await;
        read_until_contains(&mut hang, "\r\n\r\n").await;
        let list = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" });
        assert_eq!(client.post(&list).await.status, 400);

        drop(hang);
        let reply = eventually_async("the ID is free again", async || {
            let reply = client.post(&list).await;
            (reply.status == 200).then_some(reply)
        })
        .await;
        assert_eq!(reply.events()[0]["result"]["tools"][0]["name"], "echo");
    });
}

/// Test that a server that the host refuses to start gives HTTP 502 with a
/// fixed text. The host error can have host details that the sandbox must
/// not see.
///   1. Send `initialize` to a granted server that the host refuses
///   2. Check HTTP 502, the fixed text and that no session ID is given
#[test]
fn refused_server_start_gives_bad_gateway() {
    block_on_local(async {
        let host = Rc::new(FakeMcpHost::default());
        let addr = serve_local(Some(&host)).await;
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" });
        let headers = [
            "Host: mcp.airlock",
            "Accept: application/json, text/event-stream",
        ];

        let reply = http(addr, "POST", "/broken", &headers, Some(&init)).await;
        assert_eq!(reply.status, 502, "{reply:?}");
        assert_eq!(reply.body, "the MCP server is not available");
        assert_eq!(reply.header("mcp-session-id"), None);
    });
}
