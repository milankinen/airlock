//! TUI runtime settings. Hard-coded today; future iterations may load
//! from a config file or CLI flags.

pub struct TuiSettings {
    /// Max HTTP request entries kept in the Monitor tab buffer. Older
    /// entries are dropped once this cap is reached.
    pub max_http_requests: usize,
    /// Max TCP connection entries kept in the Monitor tab buffer.
    pub max_tcp_connections: usize,
}

impl Default for TuiSettings {
    fn default() -> Self {
        Self {
            max_http_requests: 100,
            max_tcp_connections: 100,
        }
    }
}
