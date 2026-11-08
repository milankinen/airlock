//! Zed specific client features.
//!
//! Tells the Zed `_meta` feature that is safe for the agent. The terminal
//! output feature only shows the output that the agent puts in its tool
//! calls. The client runs no command for it.

/// Client `_meta` feature of Zed that the agent gets in restrict mode.
pub(super) const META_NAMES: &[&str] = &["terminal_output"];
