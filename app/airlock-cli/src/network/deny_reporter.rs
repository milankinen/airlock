//! Host → guest deny notification.
//!
//! Every denied TCP/HTTP/socket connection fires a one-way
//! `Supervisor.report_deny(epoch)` RPC at the in-VM supervisor. The
//! supervisor caches the latest timestamp and exposes it on an in-guest
//! HTTP endpoint so tools inside the sandbox can detect when they've hit
//! a policy block without a host round-trip.
//!
//! Reports are sent by a task that the session owns (see
//! [`DenyReporter::attach`]). Before it is attached, `report()` is a
//! silent no-op, which keeps tests (no supervisor) and the pre-boot
//! window honest.

use std::future::Future;
use std::rc::Rc;

use airlock_common::supervisor_capnp::supervisor;
use tokio::sync::watch;

/// Late-bound deny notifier. `report()` records the latest deny time; the
/// task returned by [`attach`](Self::attach) sends it to the guest. The
/// guest keeps only the latest timestamp, so reports that arrive while an
/// RPC is in flight are coalesced into one.
pub struct DenyReporter {
    latest: watch::Sender<u64>,
}

impl DenyReporter {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            latest: watch::channel(0).0,
        })
    }

    /// Return the task that forwards deny reports to `client`. The caller
    /// owns the task; it ends when the network is dropped. Reports made
    /// before this call are not sent.
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

    /// Record a deny now. Never blocks or fails: a deny notification that
    /// doesn't reach the guest is a visibility issue, not a correctness
    /// issue, and shouldn't block or fail the original deny path.
    pub fn report(&self) {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.latest.send_replace(epoch);
    }
}
