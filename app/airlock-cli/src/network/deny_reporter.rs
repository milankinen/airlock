//! Deny notifications to the guest.
//!
//! Tells the guest about denied connections. Tools in the sandbox can then see
//! a policy block with no round trip to the host.

use std::future::Future;
use std::rc::Rc;

use airlock_common::supervisor_capnp::supervisor;
use tokio::sync::watch;

/// Deny notifier that connects to the guest late.
///
/// [`report`](Self::report) records the latest deny time. The task from
/// [`attach`](Self::attach) sends it to the guest with a one-way
/// `Supervisor.report_deny(epoch)` RPC. The supervisor keeps the latest
/// timestamp and shows it on an HTTP endpoint in the guest.
///
/// The guest keeps only the latest timestamp. Thus reports that arrive
/// while an RPC is in progress become one report.
///
/// Before the task is attached, `report()` does nothing. This is correct
/// for tests (no supervisor) and for the time before boot.
pub struct DenyReporter {
    latest: watch::Sender<u64>,
}

impl DenyReporter {
    /// Make a notifier that is not attached.
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            latest: watch::channel(0).0,
        })
    }

    /// Get the task that sends deny reports to `client`.
    /// The caller owns the task. The task stops when the network is
    /// dropped. Reports from before this call are not sent.
    pub fn attach(&self, client: supervisor::Client) -> impl Future<Output = ()> + 'static {
        let mut latest = self.latest.subscribe();
        async move {
            while latest.changed().await.is_ok() {
                let epoch = *latest.borrow_and_update();
                let mut req = client.report_deny_request();
                req.get().set_epoch(epoch);
                if let Err(e) = req.send().promise.await {
                    tracing::debug!("report_deny: {e}");
                }
            }
        }
    }

    /// Record a deny now. This never blocks or fails. A deny notification
    /// that does not get to the guest is a visibility problem, not a
    /// correctness problem. It must not block or stop the deny path.
    pub fn report(&self) {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.latest.send_replace(epoch);
    }
}
