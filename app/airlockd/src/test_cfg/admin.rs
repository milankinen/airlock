//! Helpers for tests of the admin service.

use std::net::SocketAddr;
use std::sync::Arc;

use airlock_test_utils::{read_until_eof, serve};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::admin::AdminState;
use crate::admin::server::router;

/// Serve the admin routes for `state` on a local port.
pub(crate) async fn serve_admin(state: Arc<AdminState>) -> SocketAddr {
    serve(router(state)).await
}

/// Send a JSON hook payload with POST, as the HTTP hooks of Claude Code do.
/// Check that the status is 200 and return the JSON response body.
pub(crate) async fn post_hook(addr: SocketAddr, path: &str, body: &Value) -> Value {
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: admin.airlock\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let response = read_until_eof(&mut stream).await;
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{response}");
    serde_json::from_str(body).unwrap()
}
