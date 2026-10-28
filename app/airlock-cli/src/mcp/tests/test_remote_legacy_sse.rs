//! Tests for remote MCP sessions over the legacy SSE transport, served to
//! the guest over RPC: the endpoint flow, the origin check of the
//! endpoint and the end of the stream.

use serde_json::json;

use crate::mcp::RemoteKind;
use crate::test_cfg::block_on_local;
use crate::test_cfg::mcp_remote::{REMOTE_NAME, Recorded, legacy_server, served_remote};
use crate::test_cfg::mcp_session::{Event, next, next_json, open, send};

/// Test that the relay sends the guest messages to the endpoint URL of the
/// stream, in order, and gives the answers from the stream to the guest.
///   1. Open a session of a legacy server with a relative endpoint URL
///   2. Send two requests and check both results in the sink
///   3. Check that the POST requests went to the endpoint in order, with
///      the owner headers
#[test]
fn messages_go_to_endpoint_and_answers_come_on_stream() {
    block_on_local(async {
        let (fake, url) = legacy_server(|_| "/messages?sid=1".into()).await;
        let (host, _stopper) = served_remote(RemoteKind::Sse, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();

        for (id, method) in [(1, "initialize"), (2, "tools/list")] {
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method});
            send(&session, &request.to_string()).await.unwrap();
        }
        for (id, method) in [(1, "initialize"), (2, "tools/list")] {
            let result = next_json(&mut events).await;
            assert_eq!(result["id"], id);
            assert_eq!(result["result"]["method"], method);
        }

        let posts: Vec<_> = fake
            .requests()
            .into_iter()
            .filter(|r| r.method == "POST")
            .collect();
        let methods: Vec<_> = posts.iter().filter_map(Recorded::rpc_method).collect();
        assert_eq!(methods, ["initialize", "tools/list"]);
        for post in &posts {
            assert_eq!(post.target, "/messages?sid=1");
            assert_eq!(post.header("authorization"), Some("Bearer secret"));
        }
    });
}

/// Test that an endpoint URL on another origin or with user information
/// is refused. The owner headers must not go to another server.
///   1. Start legacy servers whose endpoint has another host, or user
///      information
///   2. Check that the open fails and that no POST is sent
#[test]
fn endpoint_on_other_origin_is_refused() {
    block_on_local(async {
        let endpoints: [fn(std::net::SocketAddr) -> String; 2] = [
            |addr| format!("http://localhost:{}/messages", addr.port()),
            |addr| format!("http://user:pw@{addr}/messages"),
        ];
        for endpoint in endpoints {
            let (fake, url) = legacy_server(endpoint).await;
            let (host, _stopper) = served_remote(RemoteKind::Sse, &url);
            let Err(err) = open(&host, REMOTE_NAME, false).await else {
                panic!("open with endpoint on other origin");
            };
            assert!(err.contains("cannot connect"), "{err}");
            assert!(!fake.requests().iter().any(|r| r.method == "POST"));
        }
    });
}

/// Test that a relative endpoint URL works when the server URL has user
/// information. The join copies the user information to the endpoint, and
/// that is the same server.
///   1. Open a session of a legacy server with user information in its URL
///      and a relative endpoint URL
///   2. Send a request and check its result in the sink
#[test]
fn relative_endpoint_with_user_information_in_server_url_is_accepted() {
    block_on_local(async {
        let (fake, url) = legacy_server(|_| "/messages".into()).await;
        let url = url.replacen("http://", "http://user:pw@", 1);
        let (host, _stopper) = served_remote(RemoteKind::Sse, &url);
        let (session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();

        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        send(&session, &request.to_string()).await.unwrap();
        assert_eq!(next_json(&mut events).await["id"], 1);
        assert!(fake.requests().iter().any(|r| r.method == "POST"));
    });
}

/// Test that the end of the event stream closes the session. The legacy
/// transport has no reconnect, so the guest must start a new session.
///   1. Open a session of a legacy server
///   2. End the event stream on the server
///   3. Check the close with the stream end reason
#[test]
fn stream_end_closes_sink() {
    block_on_local(async {
        let (fake, url) = legacy_server(|_| "/messages".into()).await;
        let (host, _stopper) = served_remote(RemoteKind::Sse, &url);
        let (_session, mut events) = open(&host, REMOTE_NAME, false).await.unwrap();

        fake.end_stream();
        assert_eq!(
            next(&mut events).await,
            Event::Closed("MCP server event stream ended".into())
        );
    });
}
