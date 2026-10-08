//! Shared state that all admin endpoints use.

use std::sync::Arc;

use super::deny_tracker::DenyTracker;
use super::tool_tracker::ToolTracker;

/// State that all admin route handlers share.
pub struct AdminState {
    /// Latest network deny time, from the host.
    pub deny_tracker: Arc<DenyTracker>,
    /// Start times of running Claude Code tool calls.
    pub tool_tracker: Arc<ToolTracker>,
}

impl AdminState {
    /// Create an empty state.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            deny_tracker: DenyTracker::new(),
            tool_tracker: ToolTracker::new(),
        })
    }
}
