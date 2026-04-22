//! Cap'n Proto RPC protocol definitions shared between the CLI host and the
//! in-VM supervisor.
//!
//! Split into three schemas:
//!
//! - [`supervisor_capnp`] — `Supervisor` RPC + process / pty / log /
//!   stats / daemon / mount primitives. Carries everything except bulk
//!   network bytes. Served by the guest on [`SUPERVISOR_PORT`].
//! - [`network_capnp`] — `NetworkProxy` + `TcpSink` + connect-target
//!   types. Served by the host on [`NETWORK_PORT`] so bulk transfers
//!   cannot head-of-line-block the supervisor channel.
//! - [`cli_capnp`] — `CliService` for `airlock exec` over a local unix
//!   socket. Reuses process primitives from `supervisor_capnp` via
//!   schema import.

#[allow(clippy::all, clippy::pedantic)]
pub mod network_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/network_capnp.rs"));
}

#[allow(clippy::all, clippy::pedantic)]
pub mod supervisor_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/supervisor_capnp.rs"));
}

#[allow(clippy::all, clippy::pedantic)]
pub mod cli_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/cli_capnp.rs"));
}

/// Virtio-vsock port the supervisor listens on inside the VM.
pub const SUPERVISOR_PORT: u32 = 1024;

/// Virtio-vsock port the supervisor listens on for the network-proxy
/// RPC. Separated from [`SUPERVISOR_PORT`] so bulk byte relays on the
/// `NetworkProxy.connect` path get their own socket buffers and can't
/// starve pty / stats / daemon traffic on the supervisor channel.
pub const NETWORK_PORT: u32 = 1025;

/// Filename of the Unix domain socket that `airlock go` creates on the host so
/// that `airlock exec` can attach sidecar processes to the running container.
pub const CLI_SOCK_FILENAME: &str = "cli.sock";

/// Read-buffer size used by every TCP/Unix relay loop that pushes bytes
/// into a `TcpSink.send` call. Bigger chunks mean fewer capnp messages
/// (less framing overhead) but do not measurably improve throughput
/// beyond this value in practice — TCP batching in the guest and
/// host kernels already coalesces bytes before they reach a relay.
pub const RELAY_CHUNK_SIZE: usize = 8 * 1024;
