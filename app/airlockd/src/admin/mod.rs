//! Guest-side admin HTTP service served at `http://admin.airlock/`.
//!
//! The service is reachable only from inside the VM. It exposes endpoints
//! that tools running in the sandbox — most notably Claude Code's HTTP
//! hooks — use to coordinate with the host's network policy engine.
//!
//! Wire-up:
//! - The name `admin.airlock` resolves to `127.0.0.1` via a reserved
//!   mapping in the guest DNS server (see `net::dns`).
//! - The server binds on `127.0.0.1:80`; loopback traffic bypasses the
//!   transparent proxy's iptables redirect, so the HTTP request lands
//!   here directly.

pub mod deny_tracker;
pub mod routes;
pub mod server;
pub mod state;
pub mod tool_tracker;

pub use deny_tracker::DenyTracker;
pub use server::start;
pub use state::AdminState;
