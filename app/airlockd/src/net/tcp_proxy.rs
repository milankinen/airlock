//! TCP proxy on a TUN device.
//!
//! Opens a TUN device, runs smoltcp's userspace TCP/IP stack on it, and
//! intercepts TCP connections to any destination IP + any port by
//! snooping incoming SYN packets and creating a matching listener
//! on-the-fly. smoltcp's built-in listener requires a specific
//! address+port up front, so we peek at each rx frame, detect SYNs, and
//! add a socket bound to the observed (dst_ip, dst_port) *before*
//! smoltcp processes the SYN.
//!
//! Accepted connections are bridged to the host via the existing
//! `NetworkProxy.connect` RPC: bytes read from the smoltcp socket are
//! forwarded to the host through a per-connection relay task, and bytes
//! the host sends back are pushed into the smoltcp socket's tx buffer.
//! Unlike the iptables REDIRECT path that this replaced, the TUN catches
//! traffic from container network namespaces too, because it sits at
//! the VM's default route rather than the netfilter OUTPUT chain.
//!
//! # Poll loop shape
//!
//! smoltcp is sync/poll-driven. We drive it from tokio like this:
//!
//! 1. `drain_rx`: non-blocking drain of the TUN fd; for each packet,
//!    parse headers and, if it's a fresh SYN for an unknown (src,dst)
//!    pair, add a new listener socket bound to (dst_ip, dst_port) to
//!    the socket set. Push the packet into the Device's rx queue.
//! 2. `poll_ingress_single` one packet at a time. Whenever it reports
//!    `SocketStateChanged`, run the FSM so accepted sockets see their
//!    new state before the next ingress step.
//! 3. `poll_egress` turns socket-buffered data + FINs into wire packets
//!    pushed into the Device's tx queue.
//! 4. `poll_maintenance` advances timers (retransmits, TIME-WAIT).
//! 5. Drain the Device's tx queue to the TUN fd.
//! 6. Sleep until any of: TUN readable, host→guest notify, smoltcp's
//!    next timer, or 100ms safety-net.
//!
//! All in one task, single-threaded. No locks — the Device, Interface,
//! SocketSet and connection tracker live on the task's stack.

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant as StdInstant};

use airlock_common::supervisor_capnp::network_proxy;
use bytes::Bytes;
use smoltcp::iface::{Config, Interface, PollIngressSingleResult, Route, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    IpAddress, IpCidr, IpListenEndpoint, IpProtocol, Ipv4Address, Ipv4Packet, TcpPacket,
};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Notify, mpsc};
use tracing::{debug, error, info};

use super::dns::DnsState;
use super::rpc_bridge::{ChannelSink, rpc_connect_tcp};
use super::tun::Tun;

/// TUN MTU. We advertise 1500 to smoltcp to match what most Linux
/// stacks negotiate; the actual frame size is whatever the kernel hands
/// us on `read`.
const MTU: usize = 1500;

const RX_BUF: usize = 16 * 1024;
const TX_BUF: usize = 16 * 1024;

/// Cap on concurrent intercepted connections. Once hit, new SYNs are
/// dropped until existing sockets close.
const MAX_CONNS: usize = 256;

/// Per-direction channel capacity. Small enough to exert backpressure
/// quickly (smoltcp's send window shrinks naturally) but large enough
/// that single-byte interactive typing doesn't stall on a full queue.
const CHAN_CAP: usize = 8;

type ConnKey = (SocketAddrV4, SocketAddrV4);

/// Per-connection state held in the poll loop.
struct Conn {
    handle: SocketHandle,
    /// Bytes from guest → host. `None` once the guest half-closed and
    /// we've drained the recv buffer — dropping the sender signals the
    /// relay agent that no more data is coming.
    to_host: Option<mpsc::Sender<Bytes>>,
    /// Bytes from host → guest. The relay agent closes its half when
    /// the host side ends, which surfaces here as `Disconnected`.
    from_host_rx: mpsc::Receiver<Bytes>,
    /// True once the relay agent has been spawned (first ESTABLISHED).
    agent_spawned: bool,
    /// Leftover bytes from a previous `send_slice` that didn't fully
    /// fit into smoltcp's tx buffer; retried next iteration.
    pending_tx: Option<Bytes>,
    /// Set when the host side (relay agent) has closed — either because
    /// the RPC connect was denied/errored or the remote host FIN'd.
    host_closed: bool,
}

/// Launch the proxy as a local task. Creates `airlock0`, brings it up,
/// addresses it, installs a test route, and enters the poll loop.
pub fn start(network: network_proxy::Client, dns: Rc<DnsState>) -> anyhow::Result<()> {
    let tun = Tun::create("airlock0")?;
    let name = tun.name().to_string();

    // Interface bring-up + test route — shells out to /sbin/ip to match
    // the rest of the networking setup (see init/linux/net.rs).
    run_ip(&["link", "set", &name, "up"])?;
    run_ip(&["addr", "add", "192.168.77.1/24", "dev", &name])?;
    // airlock0 becomes the default route for the VM: every outbound TCP
    // that isn't loopback-local ends up in the smoltcp stack and, from
    // there, is relayed to the host via `NetworkProxy.connect`.
    run_ip(&["route", "add", "default", "dev", &name])?;
    // Loose reverse-path filter on airlock0 — smoltcp replies come back
    // with src=10.77.0.x which isn't owned by this interface, so strict
    // RPF would drop them.
    let _ = std::fs::write(format!("/proc/sys/net/ipv4/conf/{name}/rp_filter"), "0");

    let iface_ip = IpCidr::new(IpAddress::v4(192, 168, 77, 1), 24);
    let fd = tun.as_raw_fd();
    let async_fd = AsyncFd::with_interest(fd, Interest::READABLE | Interest::WRITABLE)?;

    let mut device = TunDevice {
        tun,
        rx_queue: VecDeque::new(),
        tx_queue: VecDeque::new(),
    };

    let cfg = Config::new(smoltcp::wire::HardwareAddress::Ip);
    let mut iface = Interface::new(cfg, &mut device, Instant::now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(iface_ip);
    });
    iface.set_any_ip(true);
    // set_any_ip only accepts packets whose destination lies within a
    // route whose gateway is one of the interface's own addresses.
    // With a `0.0.0.0/0` gateway route we accept connections to *any*
    // destination IP — the kernel delivers every egress packet here.
    iface.routes_mut().update(|routes| {
        let _ = routes.push(Route {
            cidr: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            via_router: IpAddress::Ipv4(Ipv4Address::new(192, 168, 77, 1)),
            preferred_until: None,
            expires_at: None,
        });
    });

    let mut sockets = SocketSet::new(Vec::new());
    let mut tracker: HashMap<ConnKey, Conn> = HashMap::new();

    info!("tcp proxy up on tun '{name}' addr={iface_ip}; intercepting all egress");

    // Shared wake-up signal: the TUN fd makes us readable via AsyncFd
    // when packets arrive, but host→guest bytes come via mpsc channels
    // that have no fd. Every ChannelSink pings this Notify when it
    // pushes bytes or closes, letting the poll loop wake out of its
    // idle sleep without a polling cadence.
    let wake = Rc::new(Notify::new());

    tokio::task::spawn_local(async move {
        let start = StdInstant::now();
        loop {
            let now = Instant::from_millis(start.elapsed().as_millis() as i64);

            // Ingress: process packets one at a time. Run the FSM
            // whenever a packet caused a state change so accepted
            // sockets see their new state before the next ingress step.
            loop {
                match iface.poll_ingress_single(now, &mut device, &mut sockets) {
                    PollIngressSingleResult::None => break,
                    PollIngressSingleResult::PacketProcessed => {}
                    PollIngressSingleResult::SocketStateChanged => {
                        run_fsm(&mut sockets, &mut tracker, &network, &dns, &wake);
                    }
                }
            }
            // One more FSM pass for timer-driven state changes + any
            // host→guest bytes that the wake-up signal delivered.
            run_fsm(&mut sockets, &mut tracker, &network, &dns, &wake);

            // Egress: turn socket-buffered data into device tx packets.
            let _ = iface.poll_egress(now, &mut device, &mut sockets);
            // Maintenance: retransmit timers, TIME-WAIT aging.
            iface.poll_maintenance(now);

            // Flush pending tx to the TUN.
            while let Some(pkt) = device.tx_queue.pop_front() {
                match device.tun.write(&pkt) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        device.tx_queue.push_front(pkt);
                        break;
                    }
                    Err(e) => {
                        error!("tun write: {e}");
                        break;
                    }
                }
            }

            // Sleep until *any* of: TUN fd readable, a host→guest
            // wake-up, smoltcp's next timer, or the 100ms safety net.
            let timer_wait = iface
                .poll_delay(now, &sockets)
                .map_or(Duration::from_millis(100), |d| {
                    Duration::from_micros(d.total_micros())
                });
            tokio::select! {
                biased;
                r = async_fd.readable() => {
                    match r {
                        Ok(mut g) => {
                            drain_rx(&mut device, &mut sockets, &mut tracker);
                            g.clear_ready();
                        }
                        Err(e) => {
                            error!("tun readable: {e}");
                            break;
                        }
                    }
                }
                () = wake.notified() => {}
                () = tokio::time::sleep(timer_wait) => {}
            }
        }
    });

    Ok(())
}

/// Per-connection state machine:
///  * On first ESTABLISHED, spawn the RPC relay agent.
///  * Drain smoltcp's recv buffer into the `to_host` channel.
///  * Pump `from_host_rx` into smoltcp's tx buffer (with leftover
///    bytes parked in `pending_tx` on partial writes).
///  * Half-close: once the guest FIN'd and recv is drained, drop
///    `to_host` to signal the agent. Once the agent closed and all
///    pending writes flushed, call `sock.close()` to FIN back.
///  * Reap fully-closed sockets out of the tracker and socket set.
fn run_fsm(
    sockets: &mut SocketSet<'static>,
    tracker: &mut HashMap<ConnKey, Conn>,
    network: &network_proxy::Client,
    dns: &Rc<DnsState>,
    wake: &Rc<Notify>,
) {
    let mut reap: Vec<ConnKey> = Vec::new();
    for (&key, conn) in tracker.iter_mut() {
        let sock = sockets.get_mut::<tcp::Socket>(conn.handle);

        // Spawn the relay agent the first time this socket is live.
        if !conn.agent_spawned && sock.may_send() {
            let (to_host_tx, to_host_rx) = mpsc::channel::<Bytes>(CHAN_CAP);
            let (from_host_tx, from_host_rx) = mpsc::channel::<Bytes>(CHAN_CAP);
            conn.to_host = Some(to_host_tx);
            conn.from_host_rx = from_host_rx;
            conn.agent_spawned = true;
            debug!("tcp-proxy accept: peer={} dst={}", key.0, key.1);
            tokio::task::spawn_local(relay_agent(
                network.clone(),
                dns.clone(),
                key.1,
                to_host_rx,
                from_host_tx,
                wake.clone(),
            ));
        }

        // Guest → Host: drain smoltcp recv into to_host channel while
        // we have both data and channel capacity.
        if let Some(tx) = conn.to_host.clone() {
            while sock.can_recv() {
                let Ok(permit) = tx.try_reserve() else { break };
                let recv = sock.recv(|buf| (buf.len(), Bytes::copy_from_slice(buf)));
                match recv {
                    Ok(bytes) if !bytes.is_empty() => permit.send(bytes),
                    _ => break,
                }
            }
        }

        // If the guest half-closed (FIN received and recv buffer
        // drained), stop feeding the agent.
        if conn.to_host.is_some() && !sock.may_recv() && !sock.can_recv() {
            conn.to_host = None;
        }

        // Host → Guest: only pump once the agent exists. Before that
        // the placeholder channel's sender has already been dropped,
        // so a try_recv would falsely report the host side closed.
        if conn.agent_spawned {
            if let Some(pending) = conn.pending_tx.take()
                && let Some(remaining) = push_to_socket(sock, pending)
            {
                conn.pending_tx = Some(remaining);
            }
            while conn.pending_tx.is_none() {
                match conn.from_host_rx.try_recv() {
                    Ok(data) => {
                        if let Some(remaining) = push_to_socket(sock, data) {
                            conn.pending_tx = Some(remaining);
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        conn.host_closed = true;
                        break;
                    }
                }
            }
        }

        // FIN to guest once host side is done and tx buffer drained.
        if conn.host_closed
            && conn.pending_tx.is_none()
            && sock.send_queue() == 0
            && sock.may_send()
        {
            sock.close();
        }

        if !sock.is_open() && !sock.is_active() {
            reap.push(key);
        }
    }
    for key in reap {
        if let Some(conn) = tracker.remove(&key) {
            sockets.remove(conn.handle);
            debug!("tcp-proxy reap: peer={} dst={}", key.0, key.1);
        }
    }
}

/// Try to push `data` into the socket's tx buffer. Returns `Some(rest)`
/// if the socket only accepted a prefix and the remainder must be
/// retried later; `None` if everything was queued (or the socket
/// rejected the write, which we treat as "give up on this chunk").
fn push_to_socket(sock: &mut tcp::Socket, data: Bytes) -> Option<Bytes> {
    match sock.send_slice(&data) {
        Ok(n) if n == data.len() => None,
        Ok(0) => Some(data),
        Ok(n) => Some(data.slice(n..)),
        Err(_) => None,
    }
}

/// Per-connection relay task. Opens a `NetworkProxy.connect` RPC to
/// the host and shuttles bytes: `to_host_rx` → `client_sink.send`, and
/// the host-side `server_sink.send` → `from_host_tx` (via the
/// `ChannelSink` that owns `from_host_tx`). Exits when either direction
/// closes; dropping the tx end signals the poll loop.
async fn relay_agent(
    network: network_proxy::Client,
    dns: Rc<DnsState>,
    dst: SocketAddrV4,
    mut to_host_rx: mpsc::Receiver<Bytes>,
    from_host_tx: mpsc::Sender<Bytes>,
    wake: Rc<Notify>,
) {
    let hostname = dns
        .reverse(*dst.ip())
        .unwrap_or_else(|| dst.ip().to_string());

    let server_sink = capnp_rpc::new_client(ChannelSink::with_notify(from_host_tx, wake));
    let client_sink = match rpc_connect_tcp(&network, &hostname, dst.port(), server_sink).await {
        Ok(sink) => sink,
        Err(e) => {
            debug!("tcp-proxy rpc {hostname}:{}: {e}", dst.port());
            return;
        }
    };

    while let Some(data) = to_host_rx.recv().await {
        let mut req = client_sink.send_request();
        req.get().set_data(&data);
        if req.send().await.is_err() {
            break;
        }
    }
    let _ = client_sink.close_request().send().promise.await;
}

/// Drain every packet from the TUN, SYN-snoop to register listeners,
/// and push into the Device's rx queue for smoltcp to consume.
fn drain_rx(
    device: &mut TunDevice,
    sockets: &mut SocketSet<'static>,
    tracker: &mut HashMap<ConnKey, Conn>,
) {
    let mut buf = [0u8; MTU];
    loop {
        match device.tun.read(&mut buf) {
            Ok(n) => {
                let pkt = &buf[..n];
                if let Some((src, dst)) = classify_tcp_syn(pkt)
                    && !tracker.contains_key(&(src, dst))
                    && tracker.len() < MAX_CONNS
                    && let Some(handle) = make_listener(sockets, dst)
                {
                    debug!("tcp-proxy listener: {src} → {dst}");
                    // Placeholder channel — replaced when the agent is
                    // spawned at the first ESTABLISHED tick. Using a
                    // channel here (rather than Option<Receiver>) means
                    // `run_fsm` can unconditionally call try_recv even
                    // before the agent exists: the buffer is empty so
                    // nothing happens.
                    let (_placeholder_tx, placeholder_rx) = mpsc::channel::<Bytes>(1);
                    tracker.insert(
                        (src, dst),
                        Conn {
                            handle,
                            to_host: None,
                            from_host_rx: placeholder_rx,
                            agent_spawned: false,
                            pending_tx: None,
                            host_closed: false,
                        },
                    );
                }
                device.rx_queue.push_back(pkt.to_vec());
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(e) => {
                error!("tun read: {e}");
                return;
            }
        }
    }
}

/// Parse an incoming IP packet and return (src, dst) if it is a fresh
/// TCP SYN (SYN set, ACK clear). Anything else returns None.
fn classify_tcp_syn(pkt: &[u8]) -> Option<(SocketAddrV4, SocketAddrV4)> {
    let ip = Ipv4Packet::new_checked(pkt).ok()?;
    if ip.next_header() != IpProtocol::Tcp {
        return None;
    }
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    if !tcp.syn() || tcp.ack() {
        return None;
    }
    let src = SocketAddrV4::new(Ipv4Addr::from(ip.src_addr().octets()), tcp.src_port());
    let dst = SocketAddrV4::new(Ipv4Addr::from(ip.dst_addr().octets()), tcp.dst_port());
    Some((src, dst))
}

/// Create a fresh TCP listener bound to the exact (dst_ip, dst_port)
/// and add it to the socket set. Returns the handle or None on error.
fn make_listener(sockets: &mut SocketSet<'static>, dst: SocketAddrV4) -> Option<SocketHandle> {
    let rx = tcp::SocketBuffer::new(vec![0; RX_BUF]);
    let tx = tcp::SocketBuffer::new(vec![0; TX_BUF]);
    let mut sock = tcp::Socket::new(rx, tx);
    let endpoint = IpListenEndpoint {
        addr: Some(IpAddress::Ipv4(Ipv4Address::from(dst.ip().octets()))),
        port: dst.port(),
    };
    if let Err(e) = sock.listen(endpoint) {
        error!("tcp-proxy listen({dst}): {e}");
        return None;
    }
    Some(sockets.add(sock))
}

struct TunDevice {
    tun: Tun,
    rx_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<Vec<u8>>,
}

impl Device for TunDevice {
    type RxToken<'a>
        = TunRx
    where
        Self: 'a;
    type TxToken<'a>
        = TunTx<'a>
    where
        Self: 'a;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let buf = self.rx_queue.pop_front()?;
        Some((TunRx(buf), TunTx(&mut self.tx_queue)))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        Some(TunTx(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }
}

struct TunRx(Vec<u8>);

impl RxToken for TunRx {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct TunTx<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for TunTx<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

fn run_ip(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("/sbin/ip").args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "ip {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
