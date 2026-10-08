//! Live network state control.
//!
//! Lets the TUI read and change the network policy while the sandbox runs.

use std::sync::Arc;

use parking_lot::RwLock;

use super::NetworkState;
use crate::config::config_values::Policy;

/// Cloneable, `Send + Sync` handle to read and change the runtime network
/// state. All methods hide the lock.
///
/// `Network` runs on the tokio current-thread runtime (it holds `Rc`s).
/// The TUI runs on its own OS thread. Thus the TUI changes the shared state
/// through this `Arc<RwLock<_>>` wrapper. The handle exposes only the
/// methods that the TUI needs. The full `Network` API stays private to the
/// network module.
#[derive(Clone)]
pub struct NetworkControl {
    state: Arc<RwLock<NetworkState>>,
}

impl NetworkControl {
    /// Make a handle for the given shared state.
    pub(super) fn new(state: Arc<RwLock<NetworkState>>) -> Self {
        Self { state }
    }

    /// Get the current top-level policy.
    pub fn policy(&self) -> Policy {
        self.state.read().policy
    }

    /// Replace the top-level policy. The change applies from the next
    /// connection that the network task processes.
    pub fn set_policy(&self, policy: Policy) {
        self.state.write().policy = policy;
    }
}

impl airlock_monitor::NetworkControl for NetworkControl {
    fn policy(&self) -> airlock_monitor::Policy {
        NetworkControl::policy(self).into()
    }

    fn set_policy(&self, policy: airlock_monitor::Policy) {
        NetworkControl::set_policy(self, policy.into());
    }
}

impl From<Policy> for airlock_monitor::Policy {
    fn from(p: Policy) -> Self {
        match p {
            Policy::AllowAlways => airlock_monitor::Policy::AllowAlways,
            Policy::AllowByDefault => airlock_monitor::Policy::AllowByDefault,
            Policy::DenyByDefault => airlock_monitor::Policy::DenyByDefault,
            Policy::DenyAlways => airlock_monitor::Policy::DenyAlways,
        }
    }
}

impl From<airlock_monitor::Policy> for Policy {
    fn from(p: airlock_monitor::Policy) -> Self {
        match p {
            airlock_monitor::Policy::AllowAlways => Policy::AllowAlways,
            airlock_monitor::Policy::AllowByDefault => Policy::AllowByDefault,
            airlock_monitor::Policy::DenyByDefault => Policy::DenyByDefault,
            airlock_monitor::Policy::DenyAlways => Policy::DenyAlways,
        }
    }
}
