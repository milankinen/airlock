//! Host-side Cap'n Proto RPC types for communicating with the in-VM supervisor.

mod logging;
mod process;
mod stdin;
mod supervisor;

pub use process::*;
pub use stdin::Stdin;
pub use supervisor::Supervisor;
