use std::sync::Arc;
use std::time::SystemTime;

use crate::{ConnectInfo, DisconnectInfo, NetworkEvent, RequestInfo, ResponseInfo, TrafficInfo};

pub(crate) fn connect(id: u64, host: &str, allowed: bool) -> NetworkEvent {
    NetworkEvent::Connect(Arc::new(ConnectInfo {
        id,
        timestamp: SystemTime::now(),
        host: host.into(),
        port: 443,
        allowed,
    }))
}

pub(crate) fn disconnect(id: u64) -> NetworkEvent {
    NetworkEvent::Disconnect(Arc::new(DisconnectInfo {
        id,
        timestamp: SystemTime::now(),
    }))
}

pub(crate) fn traffic(id: u64, up: u64, down: u64) -> NetworkEvent {
    NetworkEvent::Traffic(Arc::new(TrafficInfo { id, up, down }))
}

/// An allowed request to `host:443` without headers.
pub(crate) fn request(id: u64, method: &str, path: &str, host: &str) -> NetworkEvent {
    request_with(id, method, path, host, true, &[])
}

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

pub(crate) fn response(id: u64, status: u16, denied: bool) -> NetworkEvent {
    response_with(id, status, denied, &[])
}

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

fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}
