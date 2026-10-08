use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};

/// Read from `stream` until the collected text contains `needle`. Panics,
/// showing what did arrive, after two seconds.
pub async fn read_until_contains<S: AsyncRead + Unpin>(stream: &mut S, needle: &str) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let text = String::from_utf8_lossy(&buf).into_owned();
        if text.contains(needle) {
            return text;
        }
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}, got: {text:?}"))
            .unwrap();
        assert!(
            n > 0,
            "stream closed while waiting for {needle:?}, got: {text:?}"
        );
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read from `stream` until EOF. Panics, showing what did arrive, after
/// two seconds.
pub async fn read_until_eof<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for EOF, got: {:?}",
                String::from_utf8_lossy(&buf)
            )
        })
        .unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}
