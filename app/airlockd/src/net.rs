//! Guest-side networking.
//!
//! Everything here runs inside the Linux VM and shells out to
//! Linux-only syscalls (ioctl on `/dev/net/tun`, `/sbin/ip`, raw
//! AF_UNIX / AF_INET sockets). The entire implementation lives in
//! private Linux-gated submodules; this file re-exports a flat
//! public API and provides compile-time stubs for other targets so
//! `cargo check` works on macOS.
//!
//! Submodule layout (all private):
//!
//! - `tcp_proxy` — smoltcp on a TUN device intercepting all TCP egress
//!   from the VM (including container netns) and relaying each flow to
//!   the host via the `NetworkProxy.connect` RPC.
//! - `host_port_forward` — per-port loopback listeners for
//!   host-published ports (guest → host).
//! - `host_socket_forward` — unix socket forwarding (guest → host).
//! - `dns` — virtual DNS server that maps hostnames to synthetic IPs.
//! - `rpc_bridge` — shared Cap'n Proto sink/relay/connect plumbing.
//! - `tun` — minimal `/dev/net/tun` wrapper.

#[cfg(target_os = "linux")]
mod dns;
#[cfg(target_os = "linux")]
mod host_port_forward;
#[cfg(target_os = "linux")]
mod host_socket_forward;
#[cfg(target_os = "linux")]
mod rpc_bridge;
#[cfg(target_os = "linux")]
mod tcp_proxy;
#[cfg(target_os = "linux")]
mod tun;

// --- Non-Linux stubs ------------------------------------------------
//
// airlockd is only ever executed inside the Linux guest VM. These
// stubs exist so the crate still type-checks on the host-side
// developer machine (macOS, etc.) without having to shard the build
// into per-target binaries.
#[cfg(not(target_os = "linux"))]
use std::rc::Rc;

#[cfg(not(target_os = "linux"))]
use airlock_common::network_capnp::{network_proxy, tcp_sink};
#[cfg(target_os = "linux")]
pub use dns::DnsState;
#[cfg(target_os = "linux")]
pub use dns::start as start_dns;
#[cfg(target_os = "linux")]
pub use host_port_forward::start as start_host_port_forward;
#[cfg(target_os = "linux")]
pub use host_socket_forward::start as start_host_socket_forward;
#[cfg(target_os = "linux")]
pub use rpc_bridge::open_local_tcp;
#[cfg(target_os = "linux")]
pub use tcp_proxy::start as start_tcp_proxy;

#[cfg(not(target_os = "linux"))]
pub struct DnsState;

#[cfg(not(target_os = "linux"))]
impl DnsState {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn start_dns(_state: Rc<DnsState>) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

#[cfg(not(target_os = "linux"))]
pub fn start_host_socket_forward(
    _network: &network_proxy::Client,
    _sockets: Vec<crate::rpc::SocketForwardConfig>,
) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn start_host_port_forward(
    _ports: &[u16],
    _network: network_proxy::Client,
) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

#[cfg(not(target_os = "linux"))]
pub fn start_tcp_proxy(_network: network_proxy::Client, _dns: Rc<DnsState>) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn open_local_tcp(
    _port: u16,
    _client: tcp_sink::Client,
) -> anyhow::Result<tcp_sink::Client> {
    unimplemented!("airlockd only runs inside the Linux VM");
}
