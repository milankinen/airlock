//! Monitor feed messages.
//!
//! Converts the network events, the guest stats and the network policy of
//! the TUI monitor to and from their RPC form.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use airlock_common::monitor_capnp::{
    self, DiffKind as RpcDiffKind, Policy as RpcPolicy, approval_request, header, network_event,
    stats_snapshot,
};
use airlock_monitor::{
    ApprovalDetail, ApprovalDiffKind, ApprovalDiffLine, ApprovalRequest, ConnectInfo,
    DisconnectInfo, NetworkEvent, Policy, RequestInfo, ResponseInfo, StatsSnapshot, TrafficInfo,
};
use tokio::sync::oneshot;

/// Convert a policy to its RPC form.
pub fn policy_to_rpc(policy: Policy) -> RpcPolicy {
    match policy {
        Policy::AllowAlways => RpcPolicy::AllowAlways,
        Policy::AllowByDefault => RpcPolicy::AllowByDefault,
        Policy::DenyByDefault => RpcPolicy::DenyByDefault,
        Policy::DenyAlways => RpcPolicy::DenyAlways,
    }
}

/// Convert a policy from its RPC form.
pub fn policy_from_rpc(policy: RpcPolicy) -> Policy {
    match policy {
        RpcPolicy::AllowAlways => Policy::AllowAlways,
        RpcPolicy::AllowByDefault => Policy::AllowByDefault,
        RpcPolicy::DenyByDefault => Policy::DenyByDefault,
        RpcPolicy::DenyAlways => Policy::DenyAlways,
    }
}

/// Write the network events `events` into the list builder `list`. The
/// list must have the length of `events`.
pub fn write_events(
    mut list: capnp::struct_list::Builder<'_, monitor_capnp::network_event::Owned>,
    events: &[NetworkEvent],
) {
    debug_assert_eq!(list.len() as usize, events.len());
    for (i, event) in (0..list.len()).zip(events) {
        write_event(list.reborrow().get(i), event);
    }
}

/// Write one network event.
fn write_event(builder: network_event::Builder<'_>, event: &NetworkEvent) {
    match event {
        NetworkEvent::Connect(c) => {
            let mut b = builder.init_connect();
            b.set_id(c.id);
            b.set_timestamp(to_nanos(c.timestamp));
            b.set_host(&c.host);
            b.set_port(c.port);
            b.set_allowed(c.allowed);
        }
        NetworkEvent::Disconnect(d) => {
            let mut b = builder.init_disconnect();
            b.set_id(d.id);
            b.set_timestamp(to_nanos(d.timestamp));
        }
        NetworkEvent::Traffic(t) => {
            let mut b = builder.init_traffic();
            b.set_id(t.id);
            b.set_up(t.up);
            b.set_down(t.down);
        }
        NetworkEvent::Request(r) => {
            let mut b = builder.init_request();
            b.set_id(r.id);
            b.set_timestamp(to_nanos(r.timestamp));
            b.set_method(&r.method);
            b.set_path(&r.path);
            b.set_host(&r.host);
            b.set_port(r.port);
            b.set_allowed(r.allowed);
            write_headers(b.init_headers(len_u32(r.headers.len())), &r.headers);
        }
        NetworkEvent::Response(r) => {
            let mut b = builder.init_response();
            b.set_id(r.id);
            b.set_status(r.status);
            b.set_denied(r.denied);
            write_headers(b.init_headers(len_u32(r.headers.len())), &r.headers);
        }
    }
}

/// Read the network events of an `events` call.
pub fn read_events(
    list: capnp::struct_list::Reader<'_, monitor_capnp::network_event::Owned>,
) -> capnp::Result<Vec<NetworkEvent>> {
    list.iter().map(read_event).collect()
}

/// Read one network event.
fn read_event(reader: network_event::Reader<'_>) -> capnp::Result<NetworkEvent> {
    Ok(match reader.which()? {
        network_event::Connect(c) => {
            let c = c?;
            NetworkEvent::Connect(Arc::new(ConnectInfo {
                id: c.get_id(),
                timestamp: from_nanos(c.get_timestamp()),
                host: c.get_host()?.to_str()?.to_string(),
                port: c.get_port(),
                allowed: c.get_allowed(),
            }))
        }
        network_event::Disconnect(d) => {
            let d = d?;
            NetworkEvent::Disconnect(Arc::new(DisconnectInfo {
                id: d.get_id(),
                timestamp: from_nanos(d.get_timestamp()),
            }))
        }
        network_event::Traffic(t) => {
            let t = t?;
            NetworkEvent::Traffic(Arc::new(TrafficInfo {
                id: t.get_id(),
                up: t.get_up(),
                down: t.get_down(),
            }))
        }
        network_event::Request(r) => {
            let r = r?;
            NetworkEvent::Request(Arc::new(RequestInfo {
                id: r.get_id(),
                timestamp: from_nanos(r.get_timestamp()),
                method: r.get_method()?.to_str()?.to_string(),
                path: r.get_path()?.to_str()?.to_string(),
                host: r.get_host()?.to_str()?.to_string(),
                port: r.get_port(),
                allowed: r.get_allowed(),
                headers: read_headers(r.get_headers()?)?,
            }))
        }
        network_event::Response(r) => {
            let r = r?;
            NetworkEvent::Response(Arc::new(ResponseInfo {
                id: r.get_id(),
                status: r.get_status(),
                headers: read_headers(r.get_headers()?)?,
                denied: r.get_denied(),
            }))
        }
    })
}

/// Write guest stats.
pub fn write_stats(mut builder: stats_snapshot::Builder<'_>, stats: &StatsSnapshot) {
    builder.set_total_bytes(stats.total_bytes);
    builder.set_used_bytes(stats.used_bytes);
    builder.set_load1(stats.load_avg.0);
    builder.set_load5(stats.load_avg.1);
    builder.set_load15(stats.load_avg.2);
    // The set fails only if the list does not fit in a message. A guest has
    // far fewer cores than that.
    let _ = builder.set_per_core(&stats.per_core[..]);
}

/// Read guest stats.
pub fn read_stats(reader: stats_snapshot::Reader<'_>) -> capnp::Result<StatsSnapshot> {
    Ok(StatsSnapshot {
        per_core: reader.get_per_core()?.iter().collect(),
        total_bytes: reader.get_total_bytes(),
        used_bytes: reader.get_used_bytes(),
        load_avg: (reader.get_load1(), reader.get_load5(), reader.get_load15()),
    })
}

/// Write an approval request.
/// Args:
///  - `builder`: RPC form of the request
///  - `request`: The request. Its reply channel stays on this side.
///  - `time_left`: Time left for the answer
pub fn write_approval(
    mut builder: approval_request::Builder<'_>,
    request: &ApprovalRequest,
    time_left: Duration,
) {
    builder.set_title(&request.title);
    builder.set_body(&request.body);
    builder.set_time_left_ms(u64::try_from(time_left.as_millis()).unwrap_or(u64::MAX));
    let len = u32::try_from(request.details.len()).unwrap_or(u32::MAX);
    let mut list = builder.init_details(len);
    for (i, detail) in (0..len).zip(&request.details) {
        let mut b = list.reborrow().get(i);
        b.set_label(&detail.label);
        b.set_value(&detail.value);
        b.set_mono(detail.mono);
        let Some(diff) = &detail.diff else {
            continue;
        };
        let len = u32::try_from(diff.len()).unwrap_or(u32::MAX);
        let mut lines = b.init_diff(len);
        for (j, line) in (0..len).zip(diff) {
            let mut l = lines.reborrow().get(j);
            l.set_kind(match line.kind {
                ApprovalDiffKind::Same => RpcDiffKind::Same,
                ApprovalDiffKind::Add => RpcDiffKind::Add,
                ApprovalDiffKind::Remove => RpcDiffKind::Remove,
            });
            l.set_text(&line.text);
        }
    }
}

/// Read an approval request.
/// Args:
///  - `reader`: RPC form of the request
///  - `reply`: Reply channel of the request on this side
///
/// Returns:
///   The request and the time left for the answer.
pub fn read_approval(
    reader: approval_request::Reader<'_>,
    reply: oneshot::Sender<bool>,
) -> capnp::Result<(ApprovalRequest, Duration)> {
    let mut details = Vec::new();
    for d in reader.get_details()? {
        let diff = if d.has_diff() {
            let lines = d.get_diff()?.iter().map(|l| {
                Ok(ApprovalDiffLine {
                    kind: match l.get_kind()? {
                        RpcDiffKind::Same => ApprovalDiffKind::Same,
                        RpcDiffKind::Add => ApprovalDiffKind::Add,
                        RpcDiffKind::Remove => ApprovalDiffKind::Remove,
                    },
                    text: l.get_text()?.to_str()?.to_string(),
                })
            });
            Some(lines.collect::<capnp::Result<Vec<_>>>()?)
        } else {
            None
        };
        details.push(ApprovalDetail {
            label: d.get_label()?.to_str()?.to_string(),
            value: d.get_value()?.to_str()?.to_string(),
            mono: d.get_mono(),
            diff,
        });
    }
    let request = ApprovalRequest {
        title: reader.get_title()?.to_str()?.to_string(),
        body: reader.get_body()?.to_str()?.to_string(),
        details,
        reply,
    };
    Ok((request, Duration::from_millis(reader.get_time_left_ms())))
}

/// Write HTTP headers into a list with the length of `headers`.
fn write_headers(
    mut list: capnp::struct_list::Builder<'_, header::Owned>,
    headers: &[(String, String)],
) {
    debug_assert_eq!(list.len() as usize, headers.len());
    for (i, (name, value)) in (0..list.len()).zip(headers) {
        let mut h = list.reborrow().get(i);
        h.set_name(name);
        h.set_value(value);
    }
}

/// Read HTTP headers.
fn read_headers(
    list: capnp::struct_list::Reader<'_, header::Owned>,
) -> capnp::Result<Vec<(String, String)>> {
    list.iter()
        .map(|h| {
            Ok((
                h.get_name()?.to_str()?.to_string(),
                h.get_value()?.to_str()?.to_string(),
            ))
        })
        .collect()
}

/// Get a list length for a builder. The saturation is safe: a list with
/// more than `u32::MAX` items does not fit in a message, and the list
/// writers stop at the shorter length.
fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Convert a time to Unix-epoch nanoseconds. Times before the epoch give 0.
fn to_nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Convert Unix-epoch nanoseconds to a time.
fn from_nanos(nanos: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(nanos)
}
