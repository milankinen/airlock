//! Claude Code tool calls.
//!
//! Remembers the start and end time of each tool call. The hook before a tool
//! call records the start. The hooks after the call record the end and keep
//! the record for some minutes. Thus duplicate hooks use the same time
//! window. Claude Code can send both the success hook and the failure
//! hook for one call, so no hook removes the record.

use std::sync::Arc;

use quick_cache::sync::Cache;

/// Maximum number of records. Claude usually has only a few tool calls
/// running at the same time. The limit caps the memory use if a bad client
/// never calls the post hooks. The cache evicts old entries with the CLOCK
/// algorithm (similar to LRU).
const CAPACITY: usize = 1000;

/// Time to keep a record after the end of the tool call, in milliseconds.
const KEEP_AFTER_END_MS: u64 = 10 * 60 * 1000;

/// Extra time after the end of a tool call that still counts as part of the
/// call, in milliseconds. The host reports denies some time after they occur.
const DENY_LAG_MS: u64 = 2000;

/// Start and end time of a tool call (Unix time in milliseconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolWindow {
    /// Start of the tool call.
    pub start: u64,
    /// End of the tool call, or `None` while it runs.
    pub end: Option<u64>,
}

impl ToolWindow {
    /// Get the last time that is part of the tool call, including the lag of
    /// the deny reports.
    /// Args:
    ///  - `now`: Current time, used while the tool call runs
    pub fn deny_end(&self, now: u64) -> u64 {
        self.end.map_or(now, |end| end.saturating_add(DENY_LAG_MS))
    }
}

/// Time windows of recent tool calls, by `tool_use_id`.
pub struct ToolTracker {
    calls: Cache<String, ToolWindow>,
}

impl ToolTracker {
    /// Create an empty tracker.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Cache::new(CAPACITY),
        })
    }

    /// Record the start time of a tool call (Unix time in milliseconds).
    pub fn record(&self, tool_use_id: &str, epoch_ms: u64) {
        // Duplicate pre hooks must not move the start of a running call.
        if self
            .calls
            .get(tool_use_id)
            .is_some_and(|w| w.end.is_none() && w.start <= epoch_ms)
        {
            return;
        }
        let window = ToolWindow {
            start: epoch_ms,
            end: None,
        };
        self.calls.insert(tool_use_id.to_string(), window);
    }

    /// Record the end of a tool call and keep the record.
    /// Args:
    ///  - `tool_use_id`: ID of the tool call
    ///  - `now`: Current time (Unix time in milliseconds)
    ///
    /// Returns:
    ///   Time window of the call, or `None` if there is no record. A second
    ///   call gives the same window as the first one.
    pub fn finish(&self, tool_use_id: &str, now: u64) -> Option<ToolWindow> {
        let mut window = self.get(tool_use_id, now)?;
        if window.end.is_none() {
            window.end = Some(now);
            self.calls.insert(tool_use_id.to_string(), window);
        }
        Some(window)
    }

    /// Get the time window of a tool call.
    /// Args:
    ///  - `tool_use_id`: ID of the tool call
    ///  - `now`: Current time (Unix time in milliseconds)
    ///
    /// Returns:
    ///   Time window, or `None` if there is no record (no pre hook, evicted,
    ///   or ended too long ago).
    pub fn get(&self, tool_use_id: &str, now: u64) -> Option<ToolWindow> {
        let window = self.calls.get(tool_use_id)?;
        if window
            .end
            .is_some_and(|end| now.saturating_sub(end) > KEEP_AFTER_END_MS)
        {
            self.calls.remove(tool_use_id);
            return None;
        }
        Some(window)
    }
}
