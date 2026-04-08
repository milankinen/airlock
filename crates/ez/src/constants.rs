use std::time::Duration;

/// Timeout for establishing a TCP connection to the real server.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for TLS handshake with the real server.
pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for connecting to a host Unix socket.
pub const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
