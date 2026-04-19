//! Host → guest deny notification.
//!
//! Every denied TCP/HTTP/socket connection fires a one-way
//! `Supervisor.report_deny(epoch)` RPC at the in-VM supervisor. The
//! supervisor caches the latest timestamp and exposes it on an in-guest
//! HTTP endpoint so tools inside the sandbox can detect when they've hit
//! a policy block without a host round-trip.
//!
//! The client is attached after the supervisor connects — before that,
//! `report()` is a silent no-op, which keeps tests (no supervisor) and
//! the pre-boot window honest.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::supervisor_capnp::supervisor;

/// Late-bound handle to the supervisor used only for fire-and-forget
/// deny notifications. Interior mutability so `Network` (stored under
/// `Rc<Network>` once it becomes the `NetworkProxy` RPC server) can
/// have its reporter wired up after construction, once the host
/// finishes the vsock handshake.
#[derive(Default)]
pub struct DenyReporter {
    client: RefCell<Option<supervisor::Client>>,
}

impl DenyReporter {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// Attach the supervisor client. Called once, right after the vsock
    /// RPC handshake completes and before the first guest request lands.
    pub fn attach(&self, client: supervisor::Client) {
        *self.client.borrow_mut() = Some(client);
    }

    /// Fire-and-forget: send a `reportDeny(now)` to the guest. Swallows
    /// errors — a deny notification that doesn't reach the guest is a
    /// visibility issue, not a correctness issue, and shouldn't block
    /// or fail the original deny path.
    pub fn report(&self) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut req = client.report_deny_request();
        req.get().set_epoch(epoch);
        tokio::task::spawn_local(async move {
            if let Err(e) = req.send().promise.await {
                tracing::debug!("report_deny: {e}");
            }
        });
    }
}
