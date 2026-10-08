//! Reverse (host-to-guest) port forwards.
//!
//! Listens on host loopback ports and relays the accepted connections into the
//! guest. The config section `[network.ports.<name>].guest` sets the forwarded
//! ports. The sign-in callbacks of the network services also use these
//! forwards, with their own connection handler.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::Context;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use super::{io, tcp};
use crate::rpc::guest_network::GuestNetwork;

/// Bound reverse forward listeners (IPv4 loopback, optional IPv6 loopback)
/// that are not yet attached to the supervisor. It also contains the guest
/// port, so the accept loops know the destination.
pub struct BoundForward {
    v4: TcpListener,
    v6: Option<TcpListener>,
    guest_port: u16,
}

/// Bind the listeners of all reverse forwards on `127.0.0.1:<host_port>`
/// and on `[::1]:<host_port>`.
///
/// Call this before the VM boots. Then a bind failure (usually
/// `EADDRINUSE`) shows before the boot output, and no sandbox starts with
/// reverse forwards that do not work.
/// Args:
///  - `forwards`: `(host_port, guest_port)` pairs.
///
/// Returns:
///   The bound forwards, or error on the first IPv4 bind error or IPv6
///   `EADDRINUSE`. Other IPv6 bind errors (for example IPv6 disabled on the host)
///   only cause a warning, and the forward uses only the IPv4 listener.
pub async fn bind(forwards: Vec<(u16, u16)>) -> anyhow::Result<Vec<BoundForward>> {
    let mut out = Vec::with_capacity(forwards.len());
    // Bind both families. A bind of `127.0.0.1` covers only IPv4 loopback.
    // On some OS and socket combinations, an IPv6 listener on the same port
    // does NOT make the IPv4 bind fail (for example Python's `http.server`
    // on `::` with `IPV6_V6ONLY=1`, or macOS with split v4/v6 slots).
    // The second bind on `[::1]` shows conflicts on both families.
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
/// to guest `guest_port`. Use it for ports that the guest requests at run
/// time (a sign-in callback). If such a port is shared with a host program,
/// the guest gets the traffic of that program.
/// Args:
///  - `host_port`: Host loopback port to listen on
///  - `guest_port`: Guest loopback port to forward to.
///
/// Returns:
///   The bound forward, or error if another socket holds the port on IPv4
///   or IPv6. A host with no IPv6 loopback gets only the IPv4 listener.
pub fn bind_exclusive(host_port: u16, guest_port: u16) -> std::io::Result<BoundForward> {
    // This function is synchronous. Thus no other task can take the port
    // between the IPv4 bind and the IPv6 bind.
    let listen = |addr: SocketAddr| -> std::io::Result<TcpListener> {
        let socket = if addr.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        // With `SO_REUSEADDR`, macOS allows a specific-address bind next to
        // a wildcard listener. Thus do not set it on macOS. Linux never
        // shares a listening port that way. On Linux, `SO_REUSEADDR` only
        // allows a new bind of a port with connections in `TIME_WAIT` (from
        // an earlier sign-in).
        #[cfg(not(target_os = "macos"))]
        socket.set_reuseaddr(true)?;
        socket.bind(addr)?;
        socket.listen(64)
    };
    let v4 = listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port))?;
    let v6 = match listen(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), host_port)) {
        Ok(l) => Some(l),
        // No IPv6 loopback on the host.
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
    /// Get the guest port that the listeners forward to.
    pub fn guest_port(&self) -> u16 {
        self.guest_port
    }
}

/// Attach the bound listeners to the guest and relay the raw bytes of each
/// connection.
/// Args:
///  - `forwards`: Bound forwards from [`bind`] or [`bind_exclusive`]
///  - `guest`: Guest network RPC client
///  - `tasks`: Task set that gets one accept loop for each listener. Each
///    loop owns its listener and its connections. Thus when the task stops,
///    the port is released and all relayed connections close.
pub fn serve(forwards: Vec<BoundForward>, guest: &GuestNetwork, tasks: &mut JoinSet<()>) {
    for forward in forwards {
        let guest = guest.clone();
        let guest_port = forward.guest_port;
        serve_with(forward, tasks, move |stream| {
            relay_raw(stream, guest_port, guest.clone())
        });
    }
}

/// Same as [`serve`], but run `handle` for each accepted connection
/// instead of the raw relay.
/// Args:
///  - `forward`: Bound forward from [`bind`] or [`bind_exclusive`]
///  - `tasks`: Task set that gets one accept loop for each listener
///  - `handle`: Connection handler.
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

/// Relay the raw bytes of `stream` to `guest_port` on the guest loopback.
/// The relay applies no rules, no policy and no interception, because the
/// host is trusted. The connection goes through the `openLocalTcp` RPC of
/// the supervisor.
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
