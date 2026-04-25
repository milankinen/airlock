//! Host-side Cap'n Proto RPC types for communicating with the in-VM supervisor.

mod logging;
mod network;
mod process;
mod stdin;
mod supervisor;

pub use network::serve_network;
pub use process::*;
pub use stdin::Stdin;
pub use supervisor::{DaemonSpec, DaemonState, MaskSpec, Supervisor};
