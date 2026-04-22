//! Guest-side networking.
//!
//! - `tcp_proxy` — smoltcp on a TUN device intercepting all TCP egress
//!   from the VM (including container netns) and relaying each flow to
//!   the host via the `NetworkProxy.connect` RPC.
//! - `host_port_forward` — per-port loopback listeners for
//!   host-published ports (guest → host).
//! - `host_socket_forward` — unix socket forwarding (guest → host).
//! - `dns` — virtual DNS server that maps hostnames to synthetic IPs.
//! - `rpc_bridge` — shared Cap'n Proto sink/relay/connect plumbing used
//!   by all of the above.
//! - `tun` — minimal `/dev/net/tun` wrapper.

pub mod dns;
pub mod host_port_forward;
pub mod host_socket_forward;
pub mod rpc_bridge;
pub mod tcp_proxy;
pub mod tun;
