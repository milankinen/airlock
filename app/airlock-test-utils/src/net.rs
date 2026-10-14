//! Local TCP endpoints for tests.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU16, Ordering};

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// First and last port that `free_port` gives.
const PORT_RANGE: (u16, u16) = (20000, 30000);

/// Get a loopback port that is free now on IPv4 and IPv6. Each call in a
/// process gives a different port.
pub fn free_port() -> u16 {
    // A port from the OS (bind to port 0) is in the ephemeral range. Then
    // a parallel test can get the same port from the OS, or use it for an
    // outgoing connection, before the caller binds it. Ports below the
    // ephemeral range from a counter prevent both. The process id moves
    // the start, so parallel test processes use different ports.
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let (first, last) = PORT_RANGE;
    let span = last - first;
    let offset = (std::process::id() % u32::from(span)) as u16;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < span, "no free test port");
        let port = first + (offset + n) % span;
        let v4 = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port));
        let v6 = std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, port));
        let v6_ok = match v6 {
            Ok(_) => true,
            // No IPv6 loopback on the host.
            Err(e) => e.kind() != std::io::ErrorKind::AddrInUse,
        };
        if v4.is_ok() && v6_ok {
            return port;
        }
    }
}

/// Start a TCP echo server on a free loopback port. Each connection gets
/// back the bytes it sends. Call inside a `LocalSet`.
/// Returns:
///   The port of the server.
pub async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    serve_echo(listener);
    port
}

/// Serve an echo on `listener`. Call inside a `LocalSet`.
/// Returns:
///   The server task. An abort closes the listener.
pub fn serve_echo(listener: TcpListener) -> JoinHandle<()> {
    tokio::task::spawn_local(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::task::spawn_local(async move {
                let (mut r, mut w) = stream.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    })
}
