//! Tests for remote MCP sessions over Streamable HTTP, served to the
//! guest over RPC: headers and session ID, event stream answers, the
//! server event stream, session end, expiry and failed requests.

use serde_json::{Value, json};

use crate::mcp::RemoteKind;
use crate::test_cfg::mcp_remote::{
    OWNER_HEADERS, PROTOCOL_VERSION, REMOTE_NAME, SESSION_ID, served_remote, streamable_server,
};
use crate::test_cfg::mcp_session::{Event, next, next_json, open, send};
use crate::test_cfg::{block_on_local, eventually_async};

/// Initialize request with the ID 1.
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"1"}}}"#;

/// Notification that ends the initialization.
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

/// JSON-RPC request text with `id` and `method`.
fn request(id: &Value, method: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method}).to_string()
}

/// Check that `message` is the relay error response for the request `id`.
fn assert_relay_error(message: &Value, id: &Value) {
    assert_eq!(message["id"], *id, "{message}");
    assert_eq!(message["error"]["code"], -32603, "{message}");
    assert_eq!(
        message["error"]["message"], "MCP server request failed",
        "{message}"
    );
}

/// Test that the relay sends the owner headers on each request, and the
/// session ID and protocol version on each request after the
/// initialization. Without them, the server refuses the session.
///   1. Open a session of a Streamable HTTP server
///   2. Send initialize, the initialized notification and tools/list
///   3. Check the initialize and tools/list results in the sink
///   4. Check the headers of the three POST requests
#[test]
fn relay_keeps_session_id_and_owner_headers_on_each_post() {
    block_on_local(async {
        let (fake, url) = streamable_server().await;
        let (host, _stopper) = served_remote(RemoteKind::Http, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();

        send(&session, INITIALIZE).await.unwrap();
        let init = next_json(&mut events).await;
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        send(&session, INITIALIZED).await.unwrap();
        send(&session, &request(&json!(2), "tools/list"))
            .await
            .unwrap();
        let tools = next_json(&mut events).await;
        assert_eq!(tools["id"], 2);
        assert_eq!(tools["result"]["tools"][0]["name"], "echo");

        // The notification and tools/list run at the same time. Thus the
        // notification can reach the server last.
        let posts = eventually_async("three POST requests", async || {
            let posts: Vec<_> = fake
                .requests()
                .into_iter()
                .filter(|r| r.method == "POST")
                .collect();
            (posts.len() == 3).then_some(posts)
        })
        .await;
        for post in &posts {
            for (name, value) in OWNER_HEADERS {
                assert_eq!(post.header(name), Some(*value), "{post:?}");
            }
            assert_eq!(
                post.header("accept"),
                Some("application/json, text/event-stream")
            );
            assert_eq!(post.header("content-type"), Some("application/json"));
            assert!(post.header("user-agent").unwrap().starts_with("airlock/"));
            let after_init = post.rpc_method().as_deref() != Some("initialize");
            let expected_id = after_init.then_some(SESSION_ID);
            assert_eq!(post.header("mcp-session-id"), expected_id, "{post:?}");
            let expected_version = after_init.then_some(PROTOCOL_VERSION);
            assert_eq!(
                post.header("mcp-protocol-version"),
                expected_version,
                "{post:?}"
            );
        }
    });
}

/// Test that the messages of an event stream answer and of the server
/// event stream reach the guest in order. Servers send progress before
/// the result, and their own messages on the separate stream.
///   1. Initialize a session and wait until the server stream is open
///   2. Send tools/call, which the server answers with an event stream
///   3. Check the progress notification and then the result in the sink
///   4. Push a message on the server stream and check it in the sink
///   5. Check the session ID on the server stream request
#[test]
fn event_stream_answers_and_server_stream_reach_sink_in_order() {
    block_on_local(async {
        let (fake, url) = streamable_server().await;
        let (host, _stopper) = served_remote(RemoteKind::Http, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();
        send(&session, INITIALIZE).await.unwrap();
        next_json(&mut events).await;
        send(&session, INITIALIZED).await.unwrap();
        eventually_async("server stream open", async || {
            fake.stream_open().then_some(())
        })
        .await;

        send(&session, &request(&json!(3), "tools/call"))
            .await
            .unwrap();
        let progress = next_json(&mut events).await;
        assert_eq!(progress["method"], "notifications/progress");
        let result = next_json(&mut events).await;
        assert_eq!(result["id"], 3);
        assert_eq!(result["result"]["content"][0]["text"], "done");

        let changed = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        fake.push(&changed);
        assert_eq!(next_json(&mut events).await, changed);

        let get = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "GET")
            .unwrap();
        assert_eq!(get.header("mcp-session-id"), Some(SESSION_ID));
        assert_eq!(get.header("accept"), Some("text/event-stream"));
    });
}

/// Test that the release of a session ends the session on the server.
/// Else the server keeps the session until it expires.
///   1. Initialize a session
///   2. Release the session and check the close in the sink
///   3. Wait for the end requests and check the DELETE with the session
///      ID and the owner headers
#[test]
fn released_session_sends_delete() {
    block_on_local(async {
        let (fake, url) = streamable_server().await;
        let (host, _stopper) = served_remote(RemoteKind::Http, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();
        send(&session, INITIALIZE).await.unwrap();
        next_json(&mut events).await;

        drop(session);
        assert_eq!(
            next(&mut events).await,
            Event::Closed("MCP session ended".into())
        );
        crate::mcp::remote::deletes_done().await;
        let deletes: Vec<_> = fake
            .requests()
            .into_iter()
            .filter(|r| r.method == "DELETE")
            .collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert_eq!(deletes[0].header("mcp-session-id"), Some(SESSION_ID));
        assert_eq!(deletes[0].header("authorization"), Some("Bearer secret"));
    });
}

/// Test that HTTP 404 for a request with a session ID stops the session.
/// The server does not know the session any more, so the guest must start
/// a new one.
///   1. Initialize a session
///   2. Send a request that the server answers with 404
///   3. Check the close with the expiry reason, and that no DELETE goes to
///      the server
#[test]
fn expired_session_closes_sink() {
    block_on_local(async {
        let (fake, url) = streamable_server().await;
        let (host, _stopper) = served_remote(RemoteKind::Http, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();
        send(&session, INITIALIZE).await.unwrap();
        next_json(&mut events).await;

        send(&session, &request(&json!(4), "expire")).await.unwrap();
        assert_eq!(
            next(&mut events).await,
            Event::Closed("MCP server session expired".into())
        );
        crate::mcp::remote::deletes_done().await;
        assert!(!fake.requests().iter().any(|r| r.method == "DELETE"));
    });
}

/// Test that an HTTP error for a request gives the guest a JSON-RPC error
/// with the request ID, and that the session continues. Else the agent
/// waits for an answer that never comes.
///   1. Initialize a session
///   2. Send a request that the server answers with HTTP 500
///   3. Check the error response with the same ID
///   4. Send tools/list and check its result
#[test]
fn http_error_for_request_gives_jsonrpc_error_with_its_id() {
    block_on_local(async {
        let (_fake, url) = streamable_server().await;
        let (host, _stopper) = served_remote(RemoteKind::Http, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();
        send(&session, INITIALIZE).await.unwrap();
        next_json(&mut events).await;

        let id = json!("req-5");
        send(&session, &request(&id, "fail")).await.unwrap();
        assert_relay_error(&next_json(&mut events).await, &id);

        send(&session, &request(&json!(6), "tools/list"))
            .await
            .unwrap();
        assert_eq!(next_json(&mut events).await["id"], 6);
    });
}

/// Test that the relay does not follow a redirect. A redirect could send
/// the owner headers to another server.
///   1. Initialize a session
///   2. Send a request that the server answers with a redirect
///   3. Check the error response with the same ID
///   4. Check that no request went to the redirect target
#[test]
fn redirect_is_not_followed() {
    block_on_local(async {
        let (fake, url) = streamable_server().await;
        let (host, _stopper) = served_remote(RemoteKind::Http, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();
        send(&session, INITIALIZE).await.unwrap();
        next_json(&mut events).await;

        send(&session, &request(&json!(7), "redirect"))
            .await
            .unwrap();
        assert_relay_error(&next_json(&mut events).await, &json!(7));
        assert!(
            !fake.requests().iter().any(|r| r.target == "/elsewhere"),
            "{:?}",
            fake.requests()
        );
    });
}
