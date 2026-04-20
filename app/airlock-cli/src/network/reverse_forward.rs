//! Host-side listener for reverse (host → guest) port forwards.
//!
//! For each `(host_port, guest_port)` pair derived from
//! `[network.ports.<name>].guest`, bind listeners on both
//! `127.0.0.1:<host_port>` AND `[::1]:<host_port>` and bridge every
//! accepted connection into the guest via the supervisor's
//! `openLocalTcp` RPC. Raw TCP relay — no rules, no policy, no
//! interception (the host is trusted).
//!
//! Binding is split from accept-loop wiring so that `bind()` failures
//! (typically `EADDRINUSE`) surface before the VM boots — there's no
//! point starting a sandbox whose reverse forwards won't work.
//!
//! Why two listeners: a single `TcpListener::bind(("127.0.0.1", port))`
//! only covers IPv4 loopback, and on some OS/socket combinations
//! (Python's `http.server` binding `::` with `IPV6_V6ONLY=1`, macOS with
//! split v4/v6 slots) an existing IPv6 listener on the same port does
//! NOT cause the IPv4 bind to fail. Explicitly binding `[::1]` as well
//! guarantees conflict detection on either family.

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::Rc;

use airlock_common::supervisor_capnp::{supervisor, tcp_sink};
use anyhow::Context;
use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{io, tcp};

/// A pre-bound reverse-forward listener pair (IPv4 loopback, optional IPv6
/// loopback) waiting to be attached to the supervisor. Carries the
/// guest-side port so the accept loops know where to bridge to.
pub struct BoundForward {
    v4: TcpListener,
    v6: Option<TcpListener>,
    guest_port: u16,
}

/// Bind every reverse-forward listener on BOTH `127.0.0.1:<host_port>`
/// and `[::1]:<host_port>`. Fails fast on any `EADDRINUSE` from either
/// family — called before the VM is booted so the user sees the failure
/// without boot noise in the way.
///
/// An IPv6 bind failure that isn't `EADDRINUSE` (e.g. IPv6 disabled on
/// the host) is logged but tolerated — we proceed with just the IPv4
/// listener.
pub async fn bind(forwards: Vec<(u16, u16)>) -> anyhow::Result<Vec<BoundForward>> {
    let mut out = Vec::with_capacity(forwards.len());
    for (host_port, guest_port) in forwards {
        let v4 = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port))
            .await
            .with_context(|| format!("bind 127.0.0.1:{host_port} for reverse port forward"))?;
        let v6 =
            match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), host_port))
                .await
            {
                Ok(l) => Some(l),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    return Err(e).with_context(|| {
                        format!("bind [::1]:{host_port} for reverse port forward")
                    });
                }
                Err(e) => {
                    warn!("bind [::1]:{host_port} failed (IPv6 unavailable?): {e}");
                    None
                }
            };
        out.push(BoundForward { v4, v6, guest_port });
    }
    Ok(out)
}

/// Attach the pre-bound listeners to the supervisor by spawning a
/// per-listener accept loop. Listeners live for the duration of the
/// enclosing tokio local set — they unbind automatically on teardown.
pub fn serve(forwards: Vec<BoundForward>, supervisor: &supervisor::Client) {
    for BoundForward { v4, v6, guest_port } in forwards {
        spawn_accept_loop(v4, guest_port, supervisor.clone());
        if let Some(v6) = v6 {
            spawn_accept_loop(v6, guest_port, supervisor.clone());
        }
    }
}

fn spawn_accept_loop(listener: TcpListener, guest_port: u16, supervisor: supervisor::Client) {
    tokio::task::spawn_local(async move {
        accept_loop(listener, guest_port, supervisor).await;
    });
}

async fn accept_loop(listener: TcpListener, guest_port: u16, supervisor: supervisor::Client) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("reverse forward accept on guest:{guest_port}: {e}");
                continue;
            }
        };
        let supervisor = supervisor.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = handle_connection(stream, guest_port, &supervisor).await {
                debug!("reverse forward conn guest:{guest_port}: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    guest_port: u16,
    supervisor: &supervisor::Client,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: io::RelayError = Rc::new(RefCell::new(None));
    let client_sink: tcp_sink::Client = capnp_rpc::new_client(io::ChannelSink::new(tx, error));

    let mut req = supervisor.open_local_tcp_request();
    req.get().set_port(guest_port);
    req.get().set_client(client_sink);

    let response = req.send().promise.await?;
    let server_sink = response.get()?.get_server()?;

    let (read, write) = stream.into_split();
    let host_transport = io::Transport {
        read: Box::new(read),
        write: Box::new(write),
        h2: false,
    };
    let rpc_io = io::RpcTransport::new(Bytes::new(), rx, server_sink);
    let (gr, gw) = tokio::io::split(rpc_io);
    let guest_transport = io::Transport {
        read: Box::new(gr),
        write: Box::new(gw),
        h2: false,
    };

    Box::pin(tcp::relay(host_transport, guest_transport)).await;
    Ok(())
}
