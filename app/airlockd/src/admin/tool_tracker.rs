//! Running Claude Code tool calls.
//!
//! Remembers the start time of each tool call that is running. The hook before
//! a tool call records the time, and the hooks after the call remove it. When
//! a tool call fails, the admin service compares the start time with the
//! latest network deny. Thus it can tell if a deny occurred while the tool ran.

use std::sync::Arc;

use quick_cache::sync::Cache;

/// Maximum number of records. Claude usually has only a few tool calls
/// running at the same time. The limit caps the memory use if a bad client
/// never calls the post hooks. The cache evicts old entries with the CLOCK
/// algorithm (similar to LRU).
const CAPACITY: usize = 1000;

/// Start times of running tool calls, by `tool_use_id`.
pub struct ToolTracker {
    starts: Cache<String, u64>,
}

impl ToolTracker {
    /// Create an empty tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            starts: Cache::new(CAPACITY),
        })
    }

    /// Record the start time of a tool call (Unix time in milliseconds).
    pub fn record(&self, tool_use_id: &str, epoch_ms: u64) {
        self.starts.insert(tool_use_id.to_string(), epoch_ms);
    }

    /// Remove the record of a tool call and return its start time. Returns
    /// `None` if there is no record (for example, the cache evicted it, or
    /// the pre hook did not run).
    pub fn take(&self, tool_use_id: &str) -> Option<u64> {
        self.starts.remove(tool_use_id).map(|(_, v)| v)
    }
}
