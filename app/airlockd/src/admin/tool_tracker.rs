//! Tracks in-flight Claude Code tool invocations keyed by `tool_use_id`.
//!
//! The `/claude/hooks/pre-tool-use` route records the start timestamp;
//! `/post-tool-use` and `/post-tool-use-failure` consume it. The failure
//! route compares it against `DenyTracker` to decide whether a deny
//! overlapped with the tool's run.

use std::sync::Arc;

use quick_cache::sync::Cache;

/// Claude realistically has a handful of tool calls in flight at once.
/// The cap is a memory ceiling against a misbehaving client that never
/// fires post-hooks — old entries are evicted LRU-ish (CLOCK).
const CAPACITY: usize = 1000;

pub struct ToolTracker {
    starts: Cache<String, u64>,
}

impl ToolTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            starts: Cache::new(CAPACITY),
        })
    }

    pub fn record(&self, tool_use_id: &str, epoch_ms: u64) {
        self.starts.insert(tool_use_id.to_string(), epoch_ms);
    }

    /// Remove the record and return the stored start timestamp, or `None`
    /// if the id was not previously recorded (e.g. evicted, or pre-hook
    /// never fired).
    pub fn take(&self, tool_use_id: &str) -> Option<u64> {
        self.starts.remove(tool_use_id).map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_missing_returns_none() {
        let t = ToolTracker::new();
        assert_eq!(t.take("missing"), None);
    }

    #[test]
    fn record_then_take_yields_start() {
        let t = ToolTracker::new();
        t.record("abc", 42);
        assert_eq!(t.take("abc"), Some(42));
        // Second take after removal yields None.
        assert_eq!(t.take("abc"), None);
    }
}
