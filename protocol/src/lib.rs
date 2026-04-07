//! Cap'n Proto RPC protocol definitions shared between the CLI host and the
//! in-VM supervisor.
//!
//! The Rust types are generated at build time from
//! `schema/supervisor.capnp` — this module re-exports them and defines the
//! well-known constants that both sides agree on.

#[allow(clippy::all, clippy::pedantic)]
pub mod supervisor_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/supervisor_capnp.rs"));
}

/// Virtio-vsock port the supervisor listens on inside the VM.
pub const SUPERVISOR_PORT: u32 = 1024;

/// Filename of the Unix domain socket that `ez go` creates on the host so
/// that `ez exec` can attach sidecar processes to the running container.
pub const CLI_SOCK_FILENAME: &str = "cli.sock";
