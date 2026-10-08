//! Admin service for sandbox tools.
//!
//! Runs an HTTP service at `http://admin.airlock/`. Only processes inside the
//! VM can access it. Sandbox tools use it to work with the network policy of
//! the host. The most important users are the HTTP hooks of Claude Code. The
//! hooks tell Claude when a network deny is the probable cause of a failed
//! tool call.

pub mod deny_tracker;
pub mod routes;
pub mod server;
pub mod state;
pub mod tool_tracker;

#[cfg(test)]
mod tests;

pub use deny_tracker::DenyTracker;
pub use server::start;
pub use state::AdminState;
