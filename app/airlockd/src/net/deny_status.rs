//! Guest-side deny-status HTTP endpoint.
//!
//! The host tells us — via `Supervisor.report_deny` — every time a network
//! request is blocked. We cache the latest timestamp in memory and expose it
//! over `GET /last_deny` on port `DENY_STATUS_PORT` so tools running inside
//! the sandbox can poll for deny activity without a round-trip to the host.
//!
//! Response format: decimal Unix-epoch seconds followed by `\n`, or an empty
//! body when no denies have been reported yet.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use airlock_common::DENY_STATUS_PORT;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use tokio::net::TcpListener;
use tracing::{info, warn};

/// In-memory store for the latest denied-request timestamp. `Arc + AtomicU64`
/// so axum's state (which requires `Send + Sync`) and the RPC handler share
/// the same cell without locks. `0` is the sentinel for "no deny yet" —
/// saves branching on `Option` and Unix epoch 0 isn't a realistic value.
#[derive(Default)]
pub struct DenyTracker {
    last_epoch: AtomicU64,
}

impl DenyTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record a deny notification from the host. Always overwrite with the
    /// latest — out-of-order reports would only happen on clock skew.
    pub fn record(&self, epoch: u64) {
        self.last_epoch.store(epoch, Ordering::Relaxed);
    }

    pub fn last(&self) -> Option<u64> {
        match self.last_epoch.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }
}

/// Bind the deny-status listener and spawn the axum server as a local task.
pub async fn start(tracker: Arc<DenyTracker>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", DENY_STATUS_PORT)).await?;
    info!("deny-status listening on port {DENY_STATUS_PORT}");

    let app = Router::new()
        .route("/last_deny", get(last_deny))
        .with_state(tracker);

    tokio::task::spawn_local(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!("deny-status server: {e}");
        }
    });

    Ok(())
}

async fn last_deny(State(tracker): State<Arc<DenyTracker>>) -> String {
    match tracker.last() {
        Some(epoch) => format!("{epoch}\n"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_before_first_report() {
        let t = DenyTracker::new();
        assert_eq!(t.last(), None);
    }

    #[test]
    fn record_updates_latest() {
        let t = DenyTracker::new();
        t.record(1000);
        assert_eq!(t.last(), Some(1000));
        t.record(2000);
        assert_eq!(t.last(), Some(2000));
    }
}
