//! Thread-safe handle for live network-state edits from the TUI.
//!
//! `Network` lives on the tokio current-thread runtime (it holds `Rc`s). The
//! TUI runs on its own OS thread, so it mutates shared state through this
//! `Arc<RwLock<_>>` wrapper. Exposes just the methods the TUI needs — the
//! wider `Network` API stays private to the network module.

use std::sync::Arc;

use parking_lot::RwLock;

use super::NetworkState;
use crate::config::config::Policy;

/// Clonable, `Send + Sync` handle the TUI uses to read and mutate runtime
/// network state. All methods hide the lock.
#[derive(Clone)]
pub struct NetworkControl {
    state: Arc<RwLock<NetworkState>>,
}

impl NetworkControl {
    pub(super) fn new(state: Arc<RwLock<NetworkState>>) -> Self {
        Self { state }
    }

    /// Current top-level policy.
    pub fn policy(&self) -> Policy {
        self.state.read().policy
    }

    /// Replace the top-level policy. Takes effect on the next connection the
    /// network task processes.
    pub fn set_policy(&self, policy: Policy) {
        self.state.write().policy = policy;
    }
}

impl airlock_tui::NetworkControl for NetworkControl {
    fn policy(&self) -> airlock_tui::Policy {
        NetworkControl::policy(self).into()
    }

    fn set_policy(&self, policy: airlock_tui::Policy) {
        NetworkControl::set_policy(self, policy.into());
    }
}

impl From<Policy> for airlock_tui::Policy {
    fn from(p: Policy) -> Self {
        match p {
            Policy::AllowAlways => airlock_tui::Policy::AllowAlways,
            Policy::AllowByDefault => airlock_tui::Policy::AllowByDefault,
            Policy::DenyByDefault => airlock_tui::Policy::DenyByDefault,
            Policy::DenyAlways => airlock_tui::Policy::DenyAlways,
        }
    }
}

impl From<airlock_tui::Policy> for Policy {
    fn from(p: airlock_tui::Policy) -> Self {
        match p {
            airlock_tui::Policy::AllowAlways => Policy::AllowAlways,
            airlock_tui::Policy::AllowByDefault => Policy::AllowByDefault,
            airlock_tui::Policy::DenyByDefault => Policy::DenyByDefault,
            airlock_tui::Policy::DenyAlways => Policy::DenyAlways,
        }
    }
}
