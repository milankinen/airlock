//! Zed specific client features.
//!
//! Keeps the Zed `_meta` feature that is safe for the agent. The terminal
//! output feature only shows the output that the agent puts in its tool
//! calls. The client runs no command for it.

use serde_json::{Map, Value};

/// Client `_meta` feature of Zed that the agent gets in restrict mode.
const META_NAMES: &[&str] = &["terminal_output"];

/// Copy the safe Zed feature from the client `_meta` to the kept `_meta`.
/// Args:
///  - `meta`: Client `_meta` features
///
/// Returns:
///   The kept `_meta` features.
pub(super) fn keep_meta(meta: &Map<String, Value>) -> Map<String, Value> {
    let mut kept = Map::new();
    for name in META_NAMES {
        if let Some(value) = meta.get(*name) {
            kept.insert((*name).into(), value.clone());
        }
    }
    kept
}
