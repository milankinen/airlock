//! Monitor feed of the sandbox process.
//!
//! Sends the network events, the guest stats and the policy changes of the
//! sandbox to the monitor clients, and lets them change the policy. Each
//! client gets its own feed while it is attached. A slow client loses
//! events. It never slows down the sandbox network.
//!
//! While a client is attached, its monitor dialog asks the user to approve
//! requests from the sandbox when the host desktop cannot show them. The
//! last client that attached asks. When it detaches or its dialog fails,
//! the client that attached before it asks.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::monitor_capnp::{monitor_control, monitor_sink};
use airlock_monitor::{ApprovalRequest, NetworkEvent, Policy};
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::approval::{MonitorDialog, MonitorSlot};
use crate::network::{NetworkControl, NetworkHandle};
use crate::rpc::Supervisor;
use crate::rpc::monitor::{
    policy_from_rpc, policy_to_rpc, write_approval, write_events, write_stats,
};

/// Maximum number of events in one sink call.
const BATCH_MAX: usize = 256;

/// Maximum time that an event waits for its sink call.
const BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Time between two guest stats polls.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Number of events that wait for the sink. When the queue is full, new
/// events are lost.
const QUEUE_MAX: usize = 4096;

/// Sources of the monitor feed of a sandbox.
#[derive(Clone)]
pub struct MonitorFeed {
    /// Live network control and event stream.
    pub network: NetworkHandle,
    /// Host project dir, as the TUI shows it.
    pub project_path: String,
    /// Place of the monitor dialogs for approval requests. It holds the
    /// dialogs of the attached clients. The last attached client asks.
    pub approvals: MonitorSlot,
}

impl MonitorFeed {
    /// Start a feed to `sink`. The feed sends events, stats and policy
    /// changes until a sink call fails, or until the client releases the
    /// returned control. Until then, the sink also shows the approval
    /// requests.
    /// Args:
    ///  - `supervisor`: Supervisor of the VM, for the guest stats
    ///  - `sink`: Receiver of the feed
    ///
    /// Returns:
    ///   Control of the feed, and the policy at the start of the feed. The
    ///   feed sends only the later changes. Give this policy to the client,
    ///   so it misses no change. The release of the control stops the feed.
    pub fn attach(
        &self,
        supervisor: Supervisor,
        sink: monitor_sink::Client,
    ) -> (monitor_control::Client, Policy) {
        let control = self.network.control();
        // Subscribe now, so the feed gets all events after this call.
        let events = self.network.events();
        let policy = control.policy().into();
        let dialog: Rc<dyn MonitorDialog> = Rc::new(RemoteDialog {
            sink: sink.clone(),
            live: Rc::new(Cell::new(true)),
        });
        self.approvals.set(dialog.clone());
        let feed = run(events, sink, supervisor, control.clone(), policy);
        let approvals = self.approvals.clone();
        let removed = dialog.clone();
        let task = tokio::task::spawn_local(async move {
            feed.await;
            // The feed stopped by itself. The client cannot answer now.
            approvals.clear_if(&removed);
        });
        let control = capnp_rpc::new_client(ControlImpl {
            control,
            feed: task.abort_handle(),
            approvals: self.approvals.clone(),
            dialog,
        });
        (control, policy)
    }
}

/// Server of the `MonitorControl` cap. Its drop stops the feed and removes
/// the dialog of the client. The drop also occurs when the client
/// disconnects.
struct ControlImpl {
    control: NetworkControl,
    feed: AbortHandle,
    approvals: MonitorSlot,
    dialog: Rc<dyn MonitorDialog>,
}

impl Drop for ControlImpl {
    fn drop(&mut self) {
        self.feed.abort();
        self.approvals.clear_if(&self.dialog);
    }
}

/// Monitor dialog of a client. It sends the request to the sink of the
/// client. It is not live after an approve call fails.
struct RemoteDialog {
    sink: monitor_sink::Client,
    live: Rc<Cell<bool>>,
}

impl MonitorDialog for RemoteDialog {
    fn show(&self, request: ApprovalRequest, deadline: Instant) -> Result<(), ApprovalRequest> {
        if !self.live.get() {
            return Err(request);
        }
        let mut req = self.sink.approve_request();
        let time_left = deadline.saturating_duration_since(Instant::now());
        write_approval(req.get().init_request(), &request, time_left);
        let mut reply = request.reply;
        let answer = req.send().promise;
        let live = self.live.clone();
        tokio::task::spawn_local(async move {
            // The caller drops the receiver at the deadline. Then the call
            // drops here, and the late answer has no effect.
            let result = tokio::select! {
                r = answer => r,
                () = reply.closed() => return,
            };
            match result.and_then(|r| Ok(r.get()?.get_allow())) {
                Ok(allow) => {
                    let _ = reply.send(allow);
                }
                // The reply drops with no answer. The caller then denies
                // the request. The client has no dialog or is gone, so the
                // next request goes to the client that attached before.
                Err(e) => {
                    tracing::info!("monitor approval failed: {e}");
                    live.set(false);
                }
            }
        });
        Ok(())
    }
}

impl monitor_control::Server for ControlImpl {
    async fn set_policy(
        self: Rc<Self>,
        params: monitor_control::SetPolicyParams,
        _results: monitor_control::SetPolicyResults,
    ) -> Result<(), capnp::Error> {
        let policy = policy_from_rpc(params.get()?.get_policy()?);
        self.control.set_policy(policy.into());
        Ok(())
    }
}

/// Run one feed until a sink call fails or the network stops.
/// Args:
///  - `events`: Network event subscription
///  - `sink`: Receiver of the feed
///  - `supervisor`: Source of the guest stats
///  - `control`: Live network policy
///  - `policy`: Policy that the client already knows
async fn run(
    events: broadcast::Receiver<NetworkEvent>,
    sink: monitor_sink::Client,
    supervisor: Supervisor,
    control: NetworkControl,
    policy: Policy,
) {
    let (tx, rx) = mpsc::channel(QUEUE_MAX);
    let result = tokio::select! {
        () = queue_events(events, tx) => Ok(()),
        r = send_events(rx, &sink, &control, policy) => r,
        r = send_stats(&sink, &supervisor) => r,
    };
    if let Err(e) = result {
        tracing::debug!("monitor feed stopped: {e}");
    }
}

/// Move network events from the subscription to the queue, until the
/// network stops. It never waits for the sink, so the subscription does
/// not lag when the sink is slow. Events that do not fit are lost.
async fn queue_events(
    mut events: broadcast::Receiver<NetworkEvent>,
    tx: mpsc::Sender<NetworkEvent>,
) {
    loop {
        match events.recv().await {
            Ok(event) => {
                // A full queue loses the event, as a lagged subscription
                // does.
                let _ = tx.try_send(event);
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Send the queued events in batches, and the policy changes.
/// Args:
///  - `rx`: Queued events
///  - `sink`: Receiver of the feed
///  - `control`: Live network policy
///  - `policy`: Policy that the client already knows
///
/// Returns:
///   Never `Ok`, unless the queue closes. Error if a sink call fails.
async fn send_events(
    mut rx: mpsc::Receiver<NetworkEvent>,
    sink: &monitor_sink::Client,
    control: &NetworkControl,
    mut policy: Policy,
) -> capnp::Result<()> {
    let mut batch = Vec::with_capacity(BATCH_MAX);
    let mut ticker = tokio::time::interval(BATCH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // Never 0: a full batch is sent at once.
        let room = BATCH_MAX - batch.len();
        tokio::select! {
            n = rx.recv_many(&mut batch, room) => {
                if n == 0 {
                    return send_batch(sink, &mut batch).await;
                }
                if batch.len() >= BATCH_MAX {
                    send_batch(sink, &mut batch).await?;
                }
            }
            _ = ticker.tick() => {
                send_batch(sink, &mut batch).await?;
                // The network has no change notice for the policy. Thus
                // read it at each tick. A read is one uncontended lock. The
                // tick also sends the waiting events, and its sink calls
                // find a closed client.
                // TODO: Add a watch channel for the policy to
                // `NetworkControl` and send the changes at once.
                let now = control.policy().into();
                if now != policy {
                    policy = now;
                    let mut req = sink.policy_request();
                    req.get().set_policy(policy_to_rpc(policy));
                    req.send().await?;
                }
            }
        }
    }
}

/// Send `batch` to the sink if it is not empty, and clear it.
async fn send_batch(
    sink: &monitor_sink::Client,
    batch: &mut Vec<NetworkEvent>,
) -> capnp::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut req = sink.events_request();
    let len = u32::try_from(batch.len()).unwrap_or(u32::MAX);
    write_events(req.get().init_list(len), batch);
    batch.clear();
    req.send().await
}

/// Poll the guest stats and send them to the sink, one time per second.
/// If a poll fails, no stats go to the sink for that second. The next tick
/// polls again.
///
/// Returns:
///   Error if a sink call fails.
async fn send_stats(sink: &monitor_sink::Client, supervisor: &Supervisor) -> capnp::Result<()> {
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let stats = match supervisor.poll_stats().await {
            Ok(snap) => airlock_monitor::StatsSnapshot {
                per_core: snap.per_core,
                total_bytes: snap.total_bytes,
                used_bytes: snap.used_bytes,
                load_avg: snap.load_avg,
            },
            Err(e) => {
                tracing::debug!("poll_stats failed: {e}");
                continue;
            }
        };
        let mut req = sink.stats_request();
        write_stats(req.get().init_snapshot(), &stats);
        req.send().await?;
    }
}
