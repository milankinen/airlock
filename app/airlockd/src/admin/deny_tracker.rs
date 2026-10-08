//! Latest network deny time.
//!
//! Remembers when the host last reported a network deny. The admin service
//! uses this time to decide if a policy deny was the likely cause of a failed
//! tool call.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Time of the latest network deny that the host reported with
/// `Supervisor.reportDeny`.
///
/// Uses `Arc` and `AtomicU64`, so the axum state (which must be
/// `Send + Sync`) and the RPC handler share the same value without locks.
#[derive(Default)]
pub struct DenyTracker {
    /// Unix time in milliseconds. `0` means "no deny yet". This removes the
    /// need for an `Option`, and Unix time 0 is not a realistic value.
    last_epoch: AtomicU64,
}

impl DenyTracker {
    /// Create a tracker with no deny recorded.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record a deny report from the host. `epoch` is Unix time in
    /// milliseconds.
    pub fn record(&self, epoch: u64) {
        // Always replace the value with the latest report. Reports come out
        // of order only on clock skew.
        self.last_epoch.store(epoch, Ordering::Relaxed);
    }

    /// Get the time of the latest deny (Unix time in milliseconds), or
    /// `None` if the host reported no deny.
    pub fn last(&self) -> Option<u64> {
        match self.last_epoch.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }
}
