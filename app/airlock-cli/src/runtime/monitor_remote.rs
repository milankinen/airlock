//! Data source of the TUI monitor.
//!
//! Gives the TUI the project dir, the network events, the guest stats and
//! the live network policy. The data comes from the monitor feed of the
//! sandbox process over its CLI socket. The client keeps the policy, so the
//! TUI reads it with no wait.
//! A policy change goes to the sandbox process in the background. The
//! sandbox process can also ask the user to approve a request in the
//! dialog of the TUI.

use std::rc::Rc;
use std::sync::Arc;

use airlock_common::cli_capnp::cli_service;
use airlock_common::monitor_capnp::{monitor_control, monitor_sink};
use airlock_monitor::{NetworkControl, NetworkEvent, Policy, StatsSnapshot};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::approval::MonitorSlot;
use crate::rpc::monitor::{policy_from_rpc, policy_to_rpc, read_approval, read_events, read_stats};

/// Number of events that wait for the TUI. When the queue is full, the
/// sink waits, and the sandbox process loses the next events.
const EVENT_QUEUE: usize = 1024;

/// Number of stats snapshots that wait for the TUI.
const STATS_QUEUE: usize = 4;

/// Longest time limit of an approval dialog.
const MAX_TIME_LEFT: std::time::Duration = std::time::Duration::from_hours(1);

/// Extra time after the time limit of the sandbox process. The sandbox
/// process must end the request first, so that its decision is the timeout
/// and not an error of this client.
const DEADLINE_SLACK: std::time::Duration = std::time::Duration::from_millis(100);

/// Data of the TUI monitor.
pub struct MonitorSource {
    /// Host project dir.
    pub project_path: String,
    /// Live network policy.
    pub control: Arc<dyn NetworkControl>,
    /// Network events.
    pub events: mpsc::Receiver<NetworkEvent>,
    /// Guest stats.
    pub stats: mpsc::Receiver<StatsSnapshot>,
    /// Tasks that feed the source. Keep them while the TUI runs. They stop
    /// when the set drops.
    pub tasks: JoinSet<()>,
}

/// Attach a monitor to the sandbox of `service`.
/// Args:
///  - `service`: CLI service of the sandbox process
///  - `approvals`: Place of the approval dialog of the TUI. The approval
///    requests of the sandbox process go to the dialog in it. While it is
///    empty, a request waits for a dialog until its time limit ends.
///
/// Returns:
///   Source with the monitor feed of the sandbox. The feed stops when the
///   source drops.
pub async fn subscribe(
    service: &cli_service::Client,
    approvals: MonitorSlot,
) -> anyhow::Result<MonitorSource> {
    let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
    let (stats_tx, stats) = mpsc::channel(STATS_QUEUE);
    // A policy push from the sandbox process can come before this task
    // reads the result. Thus the result sets the policy only if no push
    // came first.
    let policy = Arc::new(Mutex::new(None));
    let sink: monitor_sink::Client = capnp_rpc::new_client(SinkImpl {
        events: events_tx,
        stats: stats_tx,
        policy: policy.clone(),
        approvals,
    });
    let mut req = service.monitor_request();
    req.get().set_sink(sink);
    let response = req.send().promise.await?;
    let result = response.get()?;
    let project_path = result.get_project_path()?.to_str()?.to_string();
    let start = policy_from_rpc(result.get_policy()?);
    policy.lock().get_or_insert(start);
    let remote = result.get_control()?;

    let (changes_tx, changes) = mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    tasks.spawn_local(send_policy_changes(remote, changes));
    Ok(MonitorSource {
        project_path,
        control: Arc::new(RemoteNetworkControl {
            policy,
            changes: changes_tx,
        }),
        events,
        stats,
        tasks,
    })
}

/// Send each policy change to the sandbox process. The task holds the
/// control cap of the feed, so the feed stops when the task stops.
async fn send_policy_changes(
    remote: monitor_control::Client,
    mut changes: mpsc::UnboundedReceiver<Policy>,
) {
    while let Some(policy) = changes.recv().await {
        let mut req = remote.set_policy_request();
        req.get().set_policy(policy_to_rpc(policy));
        if let Err(e) = req.send().promise.await {
            tracing::warn!("monitor policy change failed: {e}");
        }
    }
}

/// Network control of a sandbox process. The TUI thread calls it.
struct RemoteNetworkControl {
    /// Last known policy of the sandbox process. It is set before the TUI
    /// gets the control.
    policy: Arc<Mutex<Option<Policy>>>,
    /// Changes for the RPC task. The TUI thread cannot use the RPC client
    /// directly.
    changes: mpsc::UnboundedSender<Policy>,
}

impl NetworkControl for RemoteNetworkControl {
    fn policy(&self) -> Policy {
        // `None` does not occur. Deny is the safe value for it.
        self.policy.lock().unwrap_or(Policy::DenyByDefault)
    }

    fn set_policy(&self, policy: Policy) {
        *self.policy.lock() = Some(policy);
        // The sandbox process can push an older policy before it gets this
        // change. Then the old value shows for a short time. The sandbox
        // process pushes this policy after the change, so the value
        // converges.
        // The task stops only when the source drops. Then no TUI uses this
        // control.
        let _ = self.changes.send(policy);
    }
}

/// Server of the monitor sink. It puts the feed into the source channels.
struct SinkImpl {
    events: mpsc::Sender<NetworkEvent>,
    stats: mpsc::Sender<StatsSnapshot>,
    policy: Arc<Mutex<Option<Policy>>>,
    approvals: MonitorSlot,
}

/// Make the error for a closed source. The sandbox process stops the feed
/// on it.
fn closed() -> capnp::Error {
    capnp::Error::disconnected("monitor source closed".to_string())
}

/// Make the error for an approval request with no answer. The sandbox
/// process denies the request on it.
fn no_answer() -> capnp::Error {
    capnp::Error::failed("the monitor dialog gave no answer".to_string())
}

impl monitor_sink::Server for SinkImpl {
    async fn events(
        self: Rc<Self>,
        params: monitor_sink::EventsParams,
    ) -> Result<(), capnp::Error> {
        for event in read_events(params.get()?.get_list()?)? {
            // Wait for room. The stream flow control then slows down the
            // feed, and the sandbox process loses events, not the TUI.
            self.events.send(event).await.map_err(|_| closed())?;
        }
        Ok(())
    }

    async fn stats(self: Rc<Self>, params: monitor_sink::StatsParams) -> Result<(), capnp::Error> {
        let stats = read_stats(params.get()?.get_snapshot()?)?;
        self.stats.send(stats).await.map_err(|_| closed())
    }

    async fn policy(
        self: Rc<Self>,
        params: monitor_sink::PolicyParams,
    ) -> Result<(), capnp::Error> {
        *self.policy.lock() = Some(policy_from_rpc(params.get()?.get_policy()?));
        Ok(())
    }

    async fn approve(
        self: Rc<Self>,
        params: monitor_sink::ApproveParams,
        mut results: monitor_sink::ApproveResults,
    ) -> Result<(), capnp::Error> {
        let (reply, answer) = oneshot::channel();
        let (request, time_left) = read_approval(params.get()?.get_request()?, reply)?;
        // A huge time limit from the sandbox process must not overflow the
        // clock.
        let deadline = Instant::now() + time_left.min(MAX_TIME_LEFT) + DEADLINE_SLACK;
        // The client can attach the feed before its TUI starts. Then wait
        // for the TUI to register its dialog.
        let dialog = self.approvals.wait(deadline).await.ok_or_else(no_answer)?;
        dialog.show(request, deadline).map_err(|_| no_answer())?;
        // At the deadline the receiver drops. Then the TUI removes the
        // dialog, and a late answer has no effect.
        match tokio::time::timeout_at(deadline, answer).await {
            Ok(Ok(allow)) => {
                results.get().set_allow(allow);
                Ok(())
            }
            Ok(Err(_)) | Err(_) => Err(no_answer()),
        }
    }
}
