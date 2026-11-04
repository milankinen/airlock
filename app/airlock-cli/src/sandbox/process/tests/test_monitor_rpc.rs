//! Tests for the monitor feed of the CLI server: network events and policy
//! changes go between the server and a monitor client. The tests use a real
//! Unix socket. Only the guest is fake.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use airlock_common::monitor_capnp::monitor_sink;
use airlock_monitor::{
    ConnectInfo, DisconnectInfo, NetworkEvent, Policy, RequestInfo, ResponseInfo, TrafficInfo,
};

use crate::approval::MonitorSlot;
use crate::runtime::{MonitorSource, subscribe};
use crate::test_cfg::cli_server::{
    CliClient, CliSandbox, GUEST_STATS, LoopbackGuest, PROJECT_PATH,
};
use crate::test_cfg::{block_on_local, eventually_async};

/// Serve the CLI socket of a sandbox with no config, and attach a monitor
/// to it.
/// Returns:
///   The sandbox, the client connection and the monitor source.
async fn attach() -> (CliSandbox, CliClient, MonitorSource) {
    let sandbox = CliSandbox::start("", false, LoopbackGuest::client()).await;
    let client = CliClient::connect(&sandbox.sock).await;
    let source = subscribe(&client.service, MonitorSlot::default())
        .await
        .unwrap();
    (sandbox, client, source)
}

/// Get the next network event of `source`, with a time limit.
async fn next_event(source: &mut MonitorSource) -> NetworkEvent {
    tokio::time::timeout(Duration::from_secs(5), source.events.recv())
        .await
        .expect("no network event")
        .expect("event channel closed")
}

/// Test that the network events of the server arrive at the monitor client
/// with all fields and in order, so that the TUI shows the same data as
/// in the server.
///   1. Attach a monitor and check the project dir
///   2. Send one event of each type on the server
///   3. Check each event and its fields on the client
#[test]
fn network_events_on_host_arrive_at_monitor_source() {
    block_on_local(async {
        let (sandbox, _client, mut source) = attach().await;
        assert_eq!(source.project_path, PROJECT_PATH);

        let time = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789);
        let headers = vec![("accept".to_string(), "*/*".to_string())];
        let sent = [
            NetworkEvent::Connect(Arc::new(ConnectInfo {
                id: 1,
                timestamp: time,
                host: "example.com".into(),
                port: 443,
                allowed: true,
            })),
            NetworkEvent::Request(Arc::new(RequestInfo {
                id: 2,
                timestamp: time,
                method: "GET".into(),
                path: "/index.html".into(),
                host: "example.com".into(),
                port: 443,
                allowed: false,
                headers: headers.clone(),
            })),
            NetworkEvent::Response(Arc::new(ResponseInfo {
                id: 2,
                status: 403,
                headers: headers.clone(),
                denied: true,
            })),
            NetworkEvent::Traffic(Arc::new(TrafficInfo {
                id: 1,
                up: 100,
                down: 2000,
            })),
            NetworkEvent::Disconnect(Arc::new(DisconnectInfo {
                id: 1,
                timestamp: time,
            })),
        ];
        for event in sent {
            sandbox.events.send(event).unwrap();
        }

        let NetworkEvent::Connect(c) = next_event(&mut source).await else {
            panic!("expected connect");
        };
        assert_eq!(
            (c.id, c.timestamp, c.host.as_str(), c.port, c.allowed),
            (1, time, "example.com", 443, true)
        );
        let NetworkEvent::Request(r) = next_event(&mut source).await else {
            panic!("expected request");
        };
        assert_eq!(
            (r.id, r.timestamp, r.method.as_str(), r.path.as_str()),
            (2, time, "GET", "/index.html")
        );
        assert_eq!(
            (r.host.as_str(), r.port, r.allowed),
            ("example.com", 443, false)
        );
        assert_eq!(r.headers, headers);
        let NetworkEvent::Response(r) = next_event(&mut source).await else {
            panic!("expected response");
        };
        assert_eq!((r.id, r.status, r.denied), (2, 403, true));
        assert_eq!(r.headers, headers);
        let NetworkEvent::Traffic(t) = next_event(&mut source).await else {
            panic!("expected traffic");
        };
        assert_eq!((t.id, t.up, t.down), (1, 100, 2000));
        let NetworkEvent::Disconnect(d) = next_event(&mut source).await else {
            panic!("expected disconnect");
        };
        assert_eq!((d.id, d.timestamp), (1, time));
    });
}

/// Test that a policy change goes both ways between the monitor and the
/// server network, so that the TUI policy menu works over the socket and
/// the TUI does not show an old policy.
///   1. Attach a monitor and check that it shows the server policy
///   2. Change the policy in the monitor
///   3. Check that the monitor shows the new policy at once, and that the
///      server network gets it
///   4. Change the policy of the server network
///   5. Check that the monitor shows the new policy
#[test]
fn policy_changes_go_both_ways() {
    block_on_local(async {
        let (sandbox, _client, source) = attach().await;
        let host_policy = || Policy::from(sandbox.network.policy());
        assert_eq!(source.control.policy(), host_policy());
        assert_ne!(host_policy(), Policy::AllowAlways);

        source.control.set_policy(Policy::AllowAlways);
        assert_eq!(source.control.policy(), Policy::AllowAlways);
        eventually_async("host policy change", async || {
            (host_policy() == Policy::AllowAlways).then_some(())
        })
        .await;

        sandbox
            .network
            .set_policy(crate::config::config_values::Policy::DenyAlways);
        eventually_async("monitor policy change", async || {
            (source.control.policy() == Policy::DenyAlways).then_some(())
        })
        .await;
    });
}

/// Test that the server stops the feed when the monitor source drops, so that
/// a closed monitor does not keep a subscription to the network events.
///   1. Attach a monitor and check that the server subscribed to the events
///   2. Drop the source and keep the connection
///   3. Check that the server ends its subscription
#[test]
fn dropped_monitor_source_stops_host_feed() {
    block_on_local(async {
        let (sandbox, _client, source) = attach().await;
        assert_eq!(sandbox.events.receiver_count(), 1);

        drop(source);
        eventually_async("host feed stop", async || {
            (sandbox.events.receiver_count() == 0).then_some(())
        })
        .await;
    });
}

/// Test that the guest stats arrive at the monitor client, so that the TUI
/// shows the CPU and memory of the guest.
///   1. Attach a monitor to a sandbox with fixed guest stats
///   2. Check the first stats snapshot on the client
#[test]
fn guest_stats_arrive_at_monitor_source() {
    block_on_local(async {
        let (_sandbox, _client, mut source) = attach().await;
        let stats = tokio::time::timeout(Duration::from_secs(5), source.stats.recv())
            .await
            .expect("no stats")
            .expect("stats channel closed");
        let (per_core, total, used, load) = GUEST_STATS;
        assert_eq!(stats.per_core, per_core);
        assert_eq!((stats.total_bytes, stats.used_bytes), (total, used));
        assert_eq!(stats.load_avg, load);
    });
}

/// Sink that refuses all calls.
struct FailingSink;

impl monitor_sink::Server for FailingSink {
    async fn events(
        self: Rc<Self>,
        _params: monitor_sink::EventsParams,
    ) -> Result<(), capnp::Error> {
        Err(capnp::Error::failed("sink refuses".to_string()))
    }

    async fn stats(self: Rc<Self>, _params: monitor_sink::StatsParams) -> Result<(), capnp::Error> {
        Err(capnp::Error::failed("sink refuses".to_string()))
    }
}

/// Test that the server stops the feed when a sink call fails, also while the
/// client keeps the control cap. A broken client then does not keep a
/// subscription to the network events.
///   1. Attach a sink that refuses all calls, and keep the control cap
///   2. Check that the server subscribed to the events
///   3. Check that the server ends its subscription
#[test]
fn failing_sink_stops_host_feed() {
    block_on_local(async {
        let sandbox = CliSandbox::start("", false, LoopbackGuest::client()).await;
        let client = CliClient::connect(&sandbox.sock).await;
        let mut req = client.service.monitor_request();
        req.get().set_sink(capnp_rpc::new_client(FailingSink));
        let response = req.send().promise.await.unwrap();
        let _control = response.get().unwrap().get_control().unwrap();
        assert_eq!(sandbox.events.receiver_count(), 1);

        eventually_async("host feed stop", async || {
            (sandbox.events.receiver_count() == 0).then_some(())
        })
        .await;
    });
}
