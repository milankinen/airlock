//! Shared state passed to every admin route handler.

use std::sync::Arc;

use super::deny_tracker::DenyTracker;
use super::tool_tracker::ToolTracker;

pub struct AdminState {
    pub deny_tracker: Arc<DenyTracker>,
    pub tool_tracker: Arc<ToolTracker>,
}

impl AdminState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            deny_tracker: DenyTracker::new(),
            tool_tracker: ToolTracker::new(),
        })
    }
}
