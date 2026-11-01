//! Recent network denies.
//!
//! Remembers the recent network denies that the host reported. The admin
//! service uses them to find the hosts that the network policy blocked
//! during a failed tool call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Maximum number of deny entries to keep. Older entries are dropped.
const CAPACITY: usize = 256;

/// Gap for repeated denies, in milliseconds. A repeated deny less than this
/// time after the time span of an entry extends the span of the entry. Thus
/// a flood of retries uses one entry and does not push out other denies.
const REPEAT_WINDOW_MS: u64 = 1000;

/// Maximum length of a host name in a target.
const MAX_HOST_LEN: usize = 253;

/// One network deny that the host reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenyEntry {
    /// Unix time in milliseconds.
    pub epoch: u64,
    /// Destination host (or guest socket path).
    pub host: String,
    /// Destination port (0 for sockets and unknown ports).
    pub port: u16,
    /// True when the default policy denied the request, so a temporary grant
    /// can allow it.
    pub grantable: bool,
}

/// Recent network denies that the host reported with
/// `Supervisor.reportDeny`.
///
/// Uses `Arc` and a mutex, so the axum state (which must be `Send + Sync`)
/// and the RPC handler share the same entries.
#[derive(Default)]
pub struct DenyTracker {
    entries: Mutex<VecDeque<Span>>,
}

/// One deny entry and the time of its last repeat (Unix time in
/// milliseconds). `entry.epoch` is the time of the first deny.
struct Span {
    entry: DenyEntry,
    last: u64,
}

impl DenyTracker {
    /// Create a tracker with no deny recorded.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record a deny report from the host.
    pub fn record(&self, entry: DenyEntry) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only the newest entry of the same key can grow. An older one
        // stays as it is.
        let newest = entries.iter_mut().rev().find(|s| {
            s.entry.host == entry.host
                && s.entry.port == entry.port
                && s.entry.grantable == entry.grantable
        });
        if let Some(span) = newest
            && entry.epoch.saturating_add(REPEAT_WINDOW_MS) > span.entry.epoch
            && entry.epoch < span.last.saturating_add(REPEAT_WINDOW_MS)
        {
            span.entry.epoch = span.entry.epoch.min(entry.epoch);
            span.last = span.last.max(entry.epoch);
            return;
        }
        if entries.len() >= CAPACITY {
            entries.pop_front();
        }
        let last = entry.epoch;
        entries.push_back(Span { entry, last });
    }

    /// Get the denies in a time window.
    /// Args:
    ///  - `from`: Start of the window, Unix time in milliseconds (inclusive)
    ///  - `to`: End of the window, Unix time in milliseconds (inclusive)
    ///
    /// Returns:
    ///   Deny entries with a deny in the window, oldest first. A repeated
    ///   deny counts too, so an entry can start before `from`.
    pub fn between(&self, from: u64, to: u64) -> Vec<DenyEntry> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .iter()
            .filter(|s| s.last >= from && s.entry.epoch <= to)
            .map(|s| s.entry.clone())
            .collect()
    }
}

/// Get the targets of the denies.
/// Args:
///  - `entries`: Deny entries, for example from [`DenyTracker::between`]
///
/// Returns:
///   `host:port` targets (only `host` if the port is not known), with no
///   duplicates, in the order of the first deny. Host names that are not
///   safe to show to the agent are skipped.
pub fn blocked_targets(entries: &[DenyEntry]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    for e in entries.iter().filter(|e| safe_host(&e.host)) {
        let target = match (e.port, e.host.contains(':')) {
            (0, _) => e.host.clone(),
            // An IPv6 literal needs brackets before the port.
            (port, true) => format!("[{}]:{port}", e.host),
            (port, false) => format!("{}:{port}", e.host),
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// Check that a host name has only host name characters and a valid length.
/// The host comes from network traffic, and its text goes to the agent.
fn safe_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= MAX_HOST_LEN
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-:[]*".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that a flood of the same deny uses one entry, and that a repeat
    /// in a later tool window is still found. The failure hook must find
    /// all hosts of the tool call.
    ///   1. Record one deny, then many copies of another deny
    ///   2. Check that both denies are kept, and the flood as one entry
    ///   3. Record a repeat at 1500 after a deny at 1000 and check that a
    ///      window from 1400 finds it
    #[test]
    fn repeated_deny_flood_uses_one_entry_and_stays_visible() {
        let tracker = DenyTracker::new();
        let deny = |epoch, host: &str| DenyEntry {
            epoch,
            host: host.into(),
            port: 443,
            grantable: true,
        };
        tracker.record(deny(1000, "first.example"));
        for i in 0..2000 {
            tracker.record(deny(1000 + i, "flood.example"));
        }
        assert_eq!(
            tracker.between(0, u64::MAX),
            [deny(1000, "first.example"), deny(1000, "flood.example")]
        );

        let tracker = DenyTracker::new();
        tracker.record(deny(1000, "a.example"));
        assert!(tracker.between(1400, 2000).is_empty());
        tracker.record(deny(1500, "a.example"));
        assert_eq!(tracker.between(1400, 2000), [deny(1000, "a.example")]);
        assert_eq!(tracker.between(0, 900), []);
    }
}
