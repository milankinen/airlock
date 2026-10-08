//! Shared RPC protocol definitions.
//!
//! Defines the protocols between the CLI on the host and the supervisor in
//! the VM, and the constants that both sides use. The protocols cover:
//!  * control of processes, terminals, logs, statistics, daemons and mounts
//!    in the VM
//!  * network traffic between the VM and the host
//!  * `airlock exec` access to a running sandbox

/// Network proxy protocol (`NetworkProxy`, `TcpSink` and connect-target
/// types). The host serves it on [`NETWORK_PORT`]. It has its own channel, so
/// bulk transfers cannot cause head-of-line blocking on the supervisor channel.
#[allow(clippy::all, clippy::pedantic)]
pub mod network_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/network_capnp.rs"));
}

/// Supervisor protocol (`Supervisor`) for process, pty, log, stats, daemon
/// and mount control. It carries all traffic except bulk network bytes. The
/// guest serves it on [`SUPERVISOR_PORT`].
#[allow(clippy::all, clippy::pedantic)]
pub mod supervisor_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/supervisor_capnp.rs"));
}

/// CLI protocol (`CliService`) for `airlock exec` over a local Unix socket.
/// It imports the process types from [`supervisor_capnp`].
#[allow(clippy::all, clippy::pedantic)]
pub mod cli_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/cli_capnp.rs"));
}

/// Virtio-vsock port the supervisor listens on inside the VM.
pub const SUPERVISOR_PORT: u32 = 1024;

/// Virtio-vsock port for the network proxy RPC. It is different from
/// [`SUPERVISOR_PORT`], so bulk byte relays on the `NetworkProxy.connect`
/// path get their own socket buffers. Thus they cannot starve the pty, stats
/// and daemon traffic on the supervisor channel.
pub const NETWORK_PORT: u32 = 1025;

/// File name of the Unix domain socket that `airlock start` creates on the host.
/// `airlock exec` uses it to attach sidecar processes to the running container.
pub const CLI_SOCK_FILENAME: &str = "cli.sock";

/// Read buffer size for each TCP/Unix relay loop that sends bytes with
/// `TcpSink.send`. Larger chunks give fewer capnp messages (less framing
/// overhead). In practice, they do not measurably increase throughput above
/// this value, because the guest and host kernels already batch TCP bytes
/// before they get to a relay.
pub const RELAY_CHUNK_SIZE: usize = 8 * 1024;

/// Guest directory for host-bridge FIFOs and shims. It is a per-boot tmpfs.
/// Thus its contents do not outlive the VM and do not go into the persisted
/// rootfs.
pub const BRIDGE_DIR: &str = "/run/airlock";

/// Guest path of the browser shim. When the boot gives browser access, the
/// host sets `$BROWSER` to this path in the sandbox env, unless the user's
/// `[env]` sets `BROWSER`.
pub const BROWSER_SHIM: &str = "/run/airlock/bin/xdg-open";

/// Guest FIFO the browser shim writes URLs to.
pub const BROWSER_FIFO: &str = "/run/airlock/browser.open";

/// Maximum size in bytes of one URL that the guest sends to the host browser.
/// OAuth authorize URLs are much smaller than this. The host applies the same
/// limit.
pub const BROWSER_URL_MAX: usize = 8192;
