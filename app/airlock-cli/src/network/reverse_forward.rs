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
//!
//! The sign-in callback forwards of the network services
//! ([`crate::services::callback`]) bind with [`bind_exclusive`] and accept
//! with [`serve_with`], but handle each connection themselves.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Context;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use super::{io, tcp};
use crate::rpc::guest_network::GuestNetwork;

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

/// Bind one forward from host `127.0.0.1:<host_port>` and `[::1]:<host_port>`
/// to guest `guest_port`, failing when any other socket holds the port on
/// either family. For ports the guest asks for at run time (a sign-in
/// callback), where sharing a port with a host program would hand that
/// program's traffic to the guest.
///
/// macOS allows a specific-address bind next to a wildcard listener when
/// `SO_REUSEADDR` is set, so it is left off there. Linux never shares a
/// listening port that way, and there `SO_REUSEADDR` only lets a port with
/// connections in `TIME_WAIT` (an earlier sign-in) be bound again.
///
/// A host without IPv6 loopback (`EADDRNOTAVAIL` / `EAFNOSUPPORT` on
/// `::1`) gets the IPv4 listener only. Synchronous: nothing awaits between
/// the check and the bind.
pub fn bind_exclusive(host_port: u16, guest_port: u16) -> std::io::Result<BoundForward> {
    let listen = |addr: SocketAddr| -> std::io::Result<TcpListener> {
        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        #[cfg(not(target_os = "macos"))]
        socket.set_reuseaddr(true)?;
        socket.bind(addr)?;
        socket.listen(64)
    };
    let v4 = listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port))?;
    let v6 = match listen(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), host_port)) {
        Ok(l) => Some(l),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
            ) =>
        {
            debug!("no IPv6 loopback for port {host_port}: {e}");
            None
        }
        Err(e) => return Err(e),
    };
    Ok(BoundForward { v4, v6, guest_port })
}

impl BoundForward {
    /// The guest port the listeners forward to.
    pub fn guest_port(&self) -> u16 {
        self.guest_port
    }
}

/// Attach the pre-bound listeners to the guest: one accept loop per
/// listener, spawned into `tasks`, relaying each connection raw. Each loop
/// owns its listener and its connections, so stopping the task unbinds the
/// port and closes every relayed connection.
pub fn serve(forwards: Vec<BoundForward>, guest: &GuestNetwork, tasks: &mut JoinSet<()>) {
    for forward in forwards {
        let guest = guest.clone();
        let guest_port = forward.guest_port;
        serve_with(forward, tasks, move |stream| {
            relay_raw(stream, guest_port, guest.clone())
        });
    }
}

/// Like [`serve`], with `handle` run for every accepted connection
/// instead of the raw relay.
pub fn serve_with<F, Fut>(forward: BoundForward, tasks: &mut JoinSet<()>, handle: F)
where
    F: Fn(TcpStream) -> Fut + Clone + 'static,
    Fut: Future<Output = anyhow::Result<()>> + 'static,
{
    let BoundForward { v4, v6, guest_port } = forward;
    for listener in std::iter::once(v4).chain(v6) {
        tasks.spawn_local(accept_loop(listener, guest_port, handle.clone()));
    }
}

/// Accept connections and run `handle` on each.
async fn accept_loop<F, Fut>(listener: TcpListener, guest_port: u16, handle: F)
where
    F: Fn(TcpStream) -> Fut,
    Fut: Future<Output = anyhow::Result<()>> + 'static,
{
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let handled = handle(stream);
                    connections.spawn_local(async move {
                        if let Err(e) = handled.await {
                            debug!("reverse forward conn guest:{guest_port}: {e}");
                        }
                    });
                }
                Err(e) => warn!("reverse forward accept on guest:{guest_port}: {e}"),
            },
            // Reap finished connections so the set does not grow.
            Some(_) = connections.join_next() => {}
        }
    }
}

/// Relay `stream` raw to `guest_port` on the guest's loopback.
async fn relay_raw(stream: TcpStream, guest_port: u16, guest: GuestNetwork) -> anyhow::Result<()> {
    let rpc_io = guest.connect(guest_port).await?;
    let (read, write) = stream.into_split();
    let host_transport = io::Transport {
        read: Box::new(read),
        write: Box::new(write),
        h2: false,
    };
    let (gr, gw) = tokio::io::split(rpc_io);
    let guest_transport = io::Transport {
        read: Box::new(gr),
        write: Box::new(gw),
        h2: false,
    };
    Box::pin(tcp::relay(host_transport, guest_transport)).await;
    Ok(())
}
