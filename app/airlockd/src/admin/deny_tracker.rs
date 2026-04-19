//! In-memory store for the latest host-reported deny timestamp.
//!
//! The host tells the guest via `Supervisor.report_deny` every time a
//! network request is blocked. The admin routes consult this to decide
//! whether a failed tool call was likely caused by a policy deny.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// `Arc + AtomicU64` so axum's state (which requires `Send + Sync`) and
/// the RPC handler share the same cell without locks. `0` is the sentinel
/// for "no deny yet" — saves branching on `Option` and Unix epoch 0 isn't
/// a realistic value.
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
