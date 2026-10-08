//! Guest networking.
//!
//! Starts the network services of the guest:
//!  * a TCP proxy that sends all outgoing TCP traffic of the VM to the host
//!  * a virtual DNS server that gives each hostname a fake IP
//!  * forwarding of host-published ports (guest to host)
//!  * forwarding of Unix sockets (guest to host)
//!
//! Also opens TCP connections from the host to guest ports.
//!
//! Only Linux has a real implementation. On other targets, stubs let the crate
//! compile.

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

#[cfg(all(test, target_os = "linux", feature = "tun-bench"))]
mod tcp_proxy_bench;

// --- Non-Linux stubs ------------------------------------------------
//
// airlockd runs only inside the Linux guest VM. These stubs let the crate
// type-check on the developer host (macOS and others), without separate
// builds for each target.
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

/// Non-Linux stub of the DNS server state.
#[cfg(not(target_os = "linux"))]
pub struct DnsState;

#[cfg(not(target_os = "linux"))]
impl DnsState {
    /// Non-Linux stub.
    pub fn new() -> Self {
        Self
    }
}

/// Non-Linux stub. Panics if called.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn start_dns(_state: Rc<DnsState>) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

/// Non-Linux stub. Panics if called.
#[cfg(not(target_os = "linux"))]
pub fn start_host_socket_forward(
    _network: &network_proxy::Client,
    _sockets: Vec<crate::rpc::SocketForwardConfig>,
) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

/// Non-Linux stub. Panics if called.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn start_host_port_forward(
    _ports: &[u16],
    _network: network_proxy::Client,
) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

/// Non-Linux stub. Panics if called.
#[cfg(not(target_os = "linux"))]
pub fn start_tcp_proxy(_network: network_proxy::Client, _dns: Rc<DnsState>) -> anyhow::Result<()> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

/// Non-Linux stub. Panics if called.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unused_async)]
pub async fn open_local_tcp(
    _port: u16,
    _client: tcp_sink::Client,
) -> anyhow::Result<tcp_sink::Client> {
    unimplemented!("airlockd only runs inside the Linux VM");
}
