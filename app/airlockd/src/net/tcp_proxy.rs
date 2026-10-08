//! Outgoing TCP proxy.
//!
//! Catches all outgoing TCP connections in the VM, including connections from
//! containers, and relays them to the host. The host then decides how to
//! handle each connection.

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::io::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant as StdInstant};

use airlock_common::network_capnp::network_proxy;
use bytes::Bytes;
use smoltcp::iface::{
    Config, Interface, PollIngressSingleResult, PollResult, Route, SocketHandle, SocketSet,
};
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

/// TUN MTU. smoltcp gets 1500, the value that most Linux stacks negotiate.
/// The real frame size is what the kernel gives on `read`.
const MTU: usize = 1500;

/// Receive buffer size of each smoltcp socket.
const RX_BUF: usize = 16 * 1024;
/// Send buffer size of each smoltcp socket.
const TX_BUF: usize = 16 * 1024;

/// Maximum number of concurrent connections. At the limit, new SYNs are
/// dropped until existing sockets close.
const MAX_CONNS: usize = 256;

/// Channel capacity for each direction. Small, so backpressure starts
/// quickly (the smoltcp send window becomes smaller). Large enough, so that
/// single-byte interactive typing does not stop on a full queue.
const CHAN_CAP: usize = 8;

/// Connection key: `(src, dst)` addresses.
type ConnKey = (SocketAddrV4, SocketAddrV4);

#[cfg(feature = "tun-bench")]
macro_rules! bench_count {
    ($counter:ident, $n:expr) => {
        stats::$counter.fetch_add($n, std::sync::atomic::Ordering::Relaxed)
    };
}
#[cfg(not(feature = "tun-bench"))]
macro_rules! bench_count {
    ($counter:ident, $n:expr) => {};
}

/// State of one connection in the poll loop.
struct Conn {
    /// smoltcp socket of the connection.
    handle: SocketHandle,
    /// Bytes from guest to host. Capacity is `CHAN_CAP`. Becomes `None`
    /// after the guest half-closed and the recv buffer is empty. The
    /// dropped sender tells the relay agent that no more data comes.
    to_host: Option<mpsc::Sender<Bytes>>,
    /// Bytes from host to guest. The relay agent closes its half when the
    /// host side ends. The poll loop then sees `Disconnected`.
    from_host_rx: mpsc::Receiver<Bytes>,
    /// `true` after the relay agent started (at the first ESTABLISHED).
    agent_spawned: bool,
    /// Bytes from a previous `send_slice` that did not fit fully into the
    /// smoltcp tx buffer. The next iteration tries them again.
    pending_tx: Option<Bytes>,
    /// Set when the host side (relay agent) closed. The cause is a denied
    /// or failed RPC connect, or a FIN from the remote host.
    host_closed: bool,
    /// Set when the relay agent is gone but the guest did not half-close
    /// yet. Nobody can accept more guest bytes, so read and drop them. This
    /// prevents a stalled window.
    discard_rx: bool,
}

/// Start the TCP proxy in a local task.
///
/// Creates the TUN device `airlock0`, enables it, gives it its address,
/// makes it the default route of the VM and starts the poll loop.
/// Args:
///  - `network`: Host network proxy client
///  - `dns`: Virtual DNS state, to map the fake IPs back to hostnames
///
/// Returns:
///   Error if the TUN device or interface setup fails.
pub fn start(network: network_proxy::Client, dns: Rc<DnsState>) -> anyhow::Result<()> {
    let tun = Tun::create("airlock0")?;
    let name = tun.name().to_string();

    // Interface bring-up and route. Uses /sbin/ip, as the rest of the
    // network setup does (see init/linux/net.rs).
    run_ip(&["link", "set", &name, "up"])?;
    run_ip(&["addr", "add", "192.168.77.1/24", "dev", &name])?;
    // airlock0 becomes the default route of the VM. All outgoing TCP that
    // is not loopback-local goes into the smoltcp stack. From there, it goes
    // to the host through `NetworkProxy.connect`.
    run_ip(&["route", "add", "default", "dev", &name])?;
    // Loose reverse-path filter on airlock0. smoltcp replies come back with
    // the original destination as src (any IP, for example a virtual DNS IP
    // from 10.2.0.0/16), not with the 192.168.77.1 address of this
    // interface. Strict RPF would drop them.
    let _ = std::fs::write(format!("/proc/sys/net/ipv4/conf/{name}/rp_filter"), "0");

    spawn_poll_loop(tun, Ipv4Addr::new(192, 168, 77, 1), 24, network, dns)
}

/// Make the smoltcp interface on a configured TUN device and start the poll
/// loop in a local task.
///
/// Separate from [`start`], so the benchmark in `tcp_proxy_bench` can run
/// the same loop on a private TUN that it configures with ioctls, not with
/// `/sbin/ip`.
/// Args:
///  - `tun`: TUN device that is up and has its address
///  - `ip`, `prefix`: Interface address and prefix length
///  - `network`: Host network proxy client
///  - `dns`: Virtual DNS state, to map the fake IPs back to hostnames
pub(crate) fn spawn_poll_loop(
    tun: Tun,
    ip: Ipv4Addr,
    prefix: u8,
    network: network_proxy::Client,
    dns: Rc<DnsState>,
) -> anyhow::Result<()> {
    let name = tun.name().to_string();
    let iface_ip = IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(ip.octets())), prefix);
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
    // set_any_ip accepts only packets to a destination in a route whose
    // gateway is an address of the interface. With a `0.0.0.0/0` gateway
    // route, the interface accepts connections to *any* destination IP. The
    // kernel sends every outgoing packet here.
    iface.routes_mut().update(|routes| {
        let _ = routes.push(Route {
            cidr: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            via_router: IpAddress::Ipv4(Ipv4Address::from(ip.octets())),
            preferred_until: None,
            expires_at: None,
        });
    });

    let mut sockets = SocketSet::new(Vec::new());
    let mut tracker: HashMap<ConnKey, Conn> = HashMap::new();

    info!("tcp proxy up on tun '{name}' addr={iface_ip}; intercepting all egress");

    // Shared wake-up signal. AsyncFd wakes the loop when packets arrive on the
    // TUN fd, but the mpsc channels have no fd. ChannelSink notifies this on
    // host-to-guest bytes, close or drop. relay_agent notifies it after it
    // takes a chunk from `to_host`, and when it exits.
    let wake = Rc::new(Notify::new());

    // Poll loop. smoltcp is sync and poll-driven. A tokio task drives it:
    //  1. `poll_ingress_single`, one packet at a time. On
    //     `SocketStateChanged`, run the FSM, so accepted sockets see their
    //     new state before the next ingress step.
    //  2. `poll_egress` makes wire packets from socket-buffered data and
    //     FINs, into the device tx queue.
    //  3. `poll_maintenance` advances the timers (retransmits, TIME-WAIT).
    //  4. Write the device tx queue to the TUN fd.
    //  5. Sleep until one of: TUN readable, a `wake` notify (host-to-guest
    //     bytes, or the relay agent freed a `to_host` slot), the next
    //     smoltcp timer, or the 100ms safety net. When the TUN is readable,
    //     `drain_rx` reads all packets into the device rx queue.
    //
    // All in one single-threaded task. No locks: the device, interface,
    // socket set and connection tracker are on the task's stack.
    tokio::task::spawn_local(async move {
        let start = StdInstant::now();
        loop {
            bench_count!(ITERS, 1);
            let now = Instant::from_millis(start.elapsed().as_millis() as i64);

            // Ingress: process packets one at a time. Run the FSM when a
            // packet caused a state change, so accepted sockets see their
            // new state before the next ingress step.
            loop {
                match iface.poll_ingress_single(now, &mut device, &mut sockets) {
                    PollIngressSingleResult::None => break,
                    PollIngressSingleResult::PacketProcessed => {}
                    PollIngressSingleResult::SocketStateChanged => {
                        run_fsm(&mut sockets, &mut tracker, &network, &dns, &wake);
                    }
                }
            }
            // One more FSM pass for timer-driven state changes and for the
            // channel progress that the wake-up signal reported.
            run_fsm(&mut sockets, &mut tracker, &network, &dns, &wake);

            // Egress: make device tx packets from socket-buffered data.
            // `poll_egress` sends at most ONE packet per socket per call.
            // Thus call it until it reports no progress, as smoltcp's own
            // `poll()` does. One call per wake-up limits each connection to
            // one MSS per loop iteration (~2.5 MiB/s at the observed wake-up
            // rate).
            while iface.poll_egress(now, &mut device, &mut sockets)
                == PollResult::SocketStateChanged
            {}
            // Maintenance: retransmit timers, TIME-WAIT aging.
            iface.poll_maintenance(now);

            // Write the pending tx packets to the TUN.
            while let Some(pkt) = device.tx_queue.pop_front() {
                match device.tun.write(&pkt) {
                    Ok(_) => {
                        bench_count!(TX_PKTS, 1);
                    }
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

            // Sleep until *one* of: TUN fd readable, a `wake` notify, the
            // next smoltcp timer, or the 100ms safety net.
            let timer_wait = iface
                .poll_delay(now, &sockets)
                .map_or(Duration::from_millis(100), |d| {
                    Duration::from_micros(d.total_micros())
                });
            tokio::select! {
                biased;
                r = async_fd.readable() => {
                    bench_count!(WAKE_FD, 1);
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
                () = wake.notified() => {
                    bench_count!(WAKE_NOTIFY, 1);
                }
                () = tokio::time::sleep(timer_wait) => {
                    bench_count!(WAKE_TIMER, 1);
                }
            }
        }
    });

    Ok(())
}

/// Run the state machine of each connection:
///  * At the first ESTABLISHED, start the RPC relay agent.
///  * Move the data of the smoltcp recv buffer into the `to_host` channel.
///    If the agent is gone, discard the guest bytes (`discard_rx`).
///  * Move the data of `from_host_rx` into the smoltcp tx buffer. After a
///    partial write, keep the remaining bytes in `pending_tx`.
///  * Half-close: after the guest FIN and an empty recv buffer, drop
///    `to_host` to tell the agent. After the agent closed and all pending
///    writes are sent, call `sock.close()` to send a FIN back.
///  * Remove fully closed sockets from the tracker and the socket set.
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

        // Start the relay agent the first time this socket is live.
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

        // Guest to host: move smoltcp recv data into the to_host channel
        // while there is data and channel capacity.
        if let Some(tx) = conn.to_host.clone() {
            while sock.can_recv() {
                let permit = match tx.try_reserve() {
                    Ok(permit) => permit,
                    Err(mpsc::error::TrySendError::Full(())) => break,
                    // Occurs only if the RPC connect failed or a host-side
                    // send failed. A normal agent exit drops `to_host_rx`
                    // only after `to_host` is already None. Nobody reads
                    // this channel again, so discard the next bytes. Then
                    // the window does not stay closed.
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        conn.to_host = None;
                        conn.discard_rx = true;
                        break;
                    }
                };
                let recv = sock.recv(|buf| (buf.len(), Bytes::copy_from_slice(buf)));
                match recv {
                    Ok(bytes) if !bytes.is_empty() => permit.send(bytes),
                    _ => break,
                }
            }
        }

        // The relay agent is gone, but there is no FIN from the guest yet.
        // Drop the guest bytes, so the window does not stay closed.
        if conn.discard_rx {
            while sock.can_recv() && sock.recv(|b| (b.len(), ())).is_ok() {}
        }

        // If the guest half-closed (FIN received and recv buffer empty),
        // send no more data to the agent.
        if conn.to_host.is_some() && !sock.may_recv() && !sock.can_recv() {
            conn.to_host = None;
        }

        // Host to guest: only after the agent exists. Before that, the
        // sender of the placeholder channel is already dropped, so a
        // try_recv would incorrectly report that the host side closed.
        if conn.agent_spawned {
            if let Some(pending) = conn.pending_tx.take()
                && let Some(remaining) = push_to_socket(sock, pending)
            {
                conn.pending_tx = Some(remaining);
            }
            while conn.pending_tx.is_none() {
                match conn.from_host_rx.try_recv() {
                    Ok(data) => {
                        bench_count!(FSM_BYTES_IN, data.len() as u64);
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

        // Send FIN to the guest after the host side is done and the tx
        // buffer is empty.
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

/// Try to put `data` into the tx buffer of the socket.
/// Returns:
///   `Some(rest)` if the socket accepted only a prefix. The caller must try
///   the rest again later. `None` if all data is in the queue, or if the
///   socket rejected the write (then the chunk is dropped).
fn push_to_socket(sock: &mut tcp::Socket, data: Bytes) -> Option<Bytes> {
    match sock.send_slice(&data) {
        Ok(n) if n == data.len() => None,
        Ok(0) => Some(data),
        Ok(n) => Some(data.slice(n..)),
        Err(_) => None,
    }
}

/// Relay task of one connection.
///
/// Opens a connection to the host with `NetworkProxy.connect` and moves the
/// bytes:
///  * `to_host_rx` to `client_sink.send`.
///  * Host-side `server_sink.send` to `from_host_tx` (through the
///    `ChannelSink` that owns `from_host_tx`).
///
/// Exits when one of the directions closes. The dropped tx end tells the
/// poll loop.
/// Args:
///  - `network`: Host network proxy client
///  - `dns`: Virtual DNS state, to map `dst` back to a hostname
///  - `dst`: Destination address from the guest
///  - `to_host_rx`: Bytes from guest to host
///  - `from_host_tx`: Bytes from host to guest
///  - `wake`: Wake-up signal of the poll loop
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

    let server_sink = capnp_rpc::new_client(ChannelSink::with_notify(from_host_tx, wake.clone()));
    let client_sink = match rpc_connect_tcp(&network, &hostname, dst.port(), server_sink).await {
        Ok(sink) => sink,
        Err(e) => {
            debug!("tcp-proxy rpc {hostname}:{}: {e}", dst.port());
            // `to_host_rx` drops here, and no agent reads it. Wake the poll
            // loop, so run_fsm sees that `to_host` is closed.
            wake.notify_one();
            return;
        }
    };

    while let Some(data) = to_host_rx.recv().await {
        // A `to_host` slot is free now. Wake the poll loop, so it can fill
        // the slot while this task waits for the RPC send below.
        wake.notify_one();
        let mut req = client_sink.send_request();
        req.get().set_data(&data);
        if req.send().await.is_err() {
            break;
        }
    }
    let _ = client_sink.close_request().send().promise.await;
    // Same as above: wake, so run_fsm sees that `to_host` is closed.
    wake.notify_one();
}

/// Read all packets from the TUN (non-blocking) into the device rx queue
/// for smoltcp.
///
/// The built-in smoltcp listener needs a specific address and port before
/// the connection starts. Thus this function looks at each packet first.
/// For a new SYN of an unknown `(src, dst)` pair, it adds a listener socket
/// bound to `(dst_ip, dst_port)`, *before* smoltcp processes the SYN.
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
                    // Placeholder channel. Replaced when the agent starts at
                    // the first ESTABLISHED tick. With a channel here (not
                    // Option<Receiver>), the field needs no Option. The
                    // sender drops at once, so a try_recv would report
                    // `Disconnected`. Thus `run_fsm` calls try_recv only
                    // after the agent starts.
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
                            discard_rx: false,
                        },
                    );
                }
                bench_count!(RX_PKTS, 1);
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

/// Parse an incoming IP packet. Returns `(src, dst)` if it is a new TCP SYN
/// (SYN set, ACK clear). Returns `None` for all other packets.
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

/// Create a new TCP listener bound to the exact `(dst_ip, dst_port)` and add
/// it to the socket set. Returns the handle, or `None` on error.
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

/// smoltcp device on the TUN. Packets go through in-memory queues, so the
/// poll loop controls all TUN reads and writes.
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

/// smoltcp receive token: one packet from the rx queue.
struct TunRx(Vec<u8>);

impl RxToken for TunRx {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

/// smoltcp transmit token: adds one packet to the tx queue.
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

/// Run `/sbin/ip` with `args`. Returns an error with stderr if it fails.
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

/// Poll loop counters for the benchmark in `tcp_proxy_bench`. They show if
/// a throughput limit comes from the iteration rate or from the work per
/// iteration.
#[cfg(feature = "tun-bench")]
pub(crate) mod stats {
    use std::sync::atomic::AtomicU64;

    /// Poll loop iterations.
    pub static ITERS: AtomicU64 = AtomicU64::new(0);
    /// Wake-ups because the TUN fd is readable.
    pub static WAKE_FD: AtomicU64 = AtomicU64::new(0);
    /// Wake-ups from the `wake` notify.
    pub static WAKE_NOTIFY: AtomicU64 = AtomicU64::new(0);
    /// Wake-ups from the timer.
    pub static WAKE_TIMER: AtomicU64 = AtomicU64::new(0);
    /// Packets written to the TUN.
    pub static TX_PKTS: AtomicU64 = AtomicU64::new(0);
    /// Packets read from the TUN.
    pub static RX_PKTS: AtomicU64 = AtomicU64::new(0);
    /// Bytes from the host that the FSM moved into smoltcp sockets.
    pub static FSM_BYTES_IN: AtomicU64 = AtomicU64::new(0);
}
