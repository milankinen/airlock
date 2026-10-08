//! Network events for tests of the monitor.

use std::sync::Arc;
use std::time::SystemTime;

use crate::{ConnectInfo, DisconnectInfo, NetworkEvent, RequestInfo, ResponseInfo, TrafficInfo};

/// A connection to `host:443` that the policy allowed or denied.
pub(crate) fn connect(id: u64, host: &str, allowed: bool) -> NetworkEvent {
    NetworkEvent::Connect(Arc::new(ConnectInfo {
        id,
        timestamp: SystemTime::now(),
        host: host.into(),
        port: 443,
        allowed,
    }))
}

/// The end of connection `id`.
pub(crate) fn disconnect(id: u64) -> NetworkEvent {
    NetworkEvent::Disconnect(Arc::new(DisconnectInfo {
        id,
        timestamp: SystemTime::now(),
    }))
}

/// Bytes sent (`up`) and received (`down`) on connection `id`.
pub(crate) fn traffic(id: u64, up: u64, down: u64) -> NetworkEvent {
    NetworkEvent::Traffic(Arc::new(TrafficInfo { id, up, down }))
}

/// An allowed request to `host:443` without headers.
pub(crate) fn request(id: u64, method: &str, path: &str, host: &str) -> NetworkEvent {
    request_with(id, method, path, host, true, &[])
}

/// A request to `host:443` with the given policy result and headers.
pub(crate) fn request_with(
    id: u64,
    method: &str,
    path: &str,
    host: &str,
    allowed: bool,
    headers: &[(&str, &str)],
) -> NetworkEvent {
    NetworkEvent::Request(Arc::new(RequestInfo {
        id,
        timestamp: SystemTime::now(),
        method: method.into(),
        path: path.into(),
        host: host.into(),
        port: 443,
        allowed,
        headers: owned(headers),
    }))
}

/// A response to request `id` without headers.
pub(crate) fn response(id: u64, status: u16, denied: bool) -> NetworkEvent {
    response_with(id, status, denied, &[])
}

/// A response to request `id` with headers. `denied` is true if a
/// middleware script denied the request.
pub(crate) fn response_with(
    id: u64,
    status: u16,
    denied: bool,
    headers: &[(&str, &str)],
) -> NetworkEvent {
    NetworkEvent::Response(Arc::new(ResponseInfo {
        id,
        status,
        headers: owned(headers),
        denied,
    }))
}

/// Copy header pairs into owned strings.
fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}
