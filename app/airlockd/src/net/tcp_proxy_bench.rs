//! Throughput benchmark of the guest half of the TCP tunnel (TUN and
//! smoltcp), without vsock and the host proxy.
//!
//! The benchmark runs the real [`tcp_proxy`] poll loop on a private TUN
//! device. A mock `NetworkProxy` in the same process sends bytes to the
//! guest as fast as the fire-and-forget write path of the CLI does. Thus
//! the result shows if a speed limit per connection is on the guest side
//! (TUN syscalls, smoltcp, poll loop, RPC channel) or on the host side.
//!
//! A TUN device needs root and CAP_NET_ADMIN. Thus two gates apply: compile
//! with the `tun-bench` feature, then run with `--ignored` in the sandbox VM.
//! The loop counters are global to the process. Run the tests one at a time,
//! so that each test gets its own counter values:
//!
//! ```sh
//! cargo test -p airlockd --release --features tun-bench tun_bench -- \
//!     --ignored --nocapture --test-threads=1
//! ```

use std::cell::Cell;
use std::net::Ipv4Addr;
use std::rc::Rc;

use airlock_common::network_capnp::{network_proxy, tcp_sink};
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::dns::DnsState;
use super::tcp_proxy::spawn_poll_loop;
use super::tun::Tun;

/// Netmask of the TUN address and of the route to the mock server.
const NETMASK: [u8; 4] = [255, 255, 255, 0];

/// Bytes that each benchmark sends through the tunnel.
const TOTAL: usize = 64 * 1024 * 1024;
/// Write size of the CLI: one TLS record is approximately 16 KiB.
const CHUNK: usize = 16 * 1024;

/// Network values of one test. Tests can run in parallel threads, so each
/// test has its own TUN device and subnet.
struct BenchNet {
    tun_name: &'static str,
    tun_ip: [u8; 4],
    /// Subnet that is routed into the TUN. The mock server is in it.
    dst_net: [u8; 4],
    dst_ip: [u8; 4],
}

/// Measure the download speed from the host to a guest TCP client through
/// the TUN stack.
///   1. Start the poll loop with a mock proxy that sends 64 MiB to the guest
///   2. Connect a TCP client through the TUN and read to EOF
///   3. Check that all bytes arrived and print the speed and loop counters
#[test]
#[ignore = "needs root + /dev/net/tun; run inside the sandbox VM"]
fn tun_bench_download() {
    let net = BenchNet {
        tun_name: "bench1",
        tun_ip: [192, 168, 88, 1],
        dst_net: [10, 99, 0, 0],
        dst_ip: [10, 99, 0, 7],
    };
    run_bench(&net, Mode::Download, |mut stream| async move {
        let mut buf = vec![0u8; 256 * 1024];
        let mut got = 0usize;
        loop {
            let n = stream.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            got += n;
        }
        assert_eq!(got, TOTAL, "stream truncated");
    });
}

/// Measure the upload speed from a guest TCP client to the host through the
/// TUN stack.
///   1. Start the poll loop with a mock proxy that counts the received bytes
///   2. Connect a TCP client through the TUN, write 64 MiB and close
///   3. Check that the proxy got all bytes and that the client gets EOF
///   4. Print the speed and loop counters
#[test]
#[ignore = "needs root + /dev/net/tun; run inside the sandbox VM"]
fn tun_bench_upload() {
    let net = BenchNet {
        tun_name: "bench2",
        tun_ip: [192, 168, 89, 1],
        dst_net: [10, 99, 1, 0],
        dst_ip: [10, 99, 1, 7],
    };
    run_bench(&net, Mode::Upload, |mut stream| async move {
        let buf = vec![0x5Au8; 256 * 1024];
        let mut sent = 0usize;
        while sent < TOTAL {
            let n = buf.len().min(TOTAL - sent);
            stream.write_all(&buf[..n]).await.expect("write");
            sent += n;
        }
        stream.shutdown().await.expect("shutdown");
        let mut tail = [0u8; 16];
        assert_eq!(stream.read(&mut tail).await.expect("eof"), 0);
    });
}

/// Run one benchmark. Create the TUN and start the poll loop with the mock
/// proxy. Then give a connected `TcpStream` to `body` and print the speed.
fn run_bench<F, Fut>(net: &BenchNet, mode: Mode, body: F)
where
    F: FnOnce(tokio::net::TcpStream) -> Fut,
    Fut: Future<Output = ()>,
{
    {
        use std::sync::atomic::Ordering::Relaxed;

        use super::tcp_proxy::stats;
        for counter in [
            &stats::ITERS,
            &stats::WAKE_FD,
            &stats::WAKE_NOTIFY,
            &stats::WAKE_TIMER,
            &stats::TX_PKTS,
            &stats::RX_PKTS,
            &stats::FSM_BYTES_IN,
        ] {
            counter.store(0, Relaxed);
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        let tun = Tun::create(net.tun_name).expect("create TUN (root + CAP_NET_ADMIN required)");
        setup_iface(net);
        // Turn off the reverse path filter, so that the kernel does not drop
        // packets that come in on the TUN.
        let _ = std::fs::write(
            format!("/proc/sys/net/ipv4/conf/{}/rp_filter", net.tun_name),
            "0",
        );

        let proxy = start_mock_proxy(mode);
        let dns = Rc::new(DnsState::new());
        spawn_poll_loop(tun, Ipv4Addr::from(net.tun_ip), 24, proxy, dns).unwrap();

        let start = std::time::Instant::now();
        let stream = tokio::net::TcpStream::connect((Ipv4Addr::from(net.dst_ip), 80))
            .await
            .expect("connect through TUN");
        body(stream).await;
        let secs = start.elapsed().as_secs_f64();
        println!(
            "{}: {} MiB in {secs:.2}s = {:.2} MiB/s",
            net.tun_name,
            TOTAL / (1024 * 1024),
            TOTAL as f64 / (1024.0 * 1024.0) / secs
        );
        {
            use std::sync::atomic::Ordering::Relaxed;

            use super::tcp_proxy::stats;
            let iters = stats::ITERS.load(Relaxed);
            println!(
                "  loop: {iters} iters ({:.0}/s), wake fd={} notify={} timer={}",
                iters as f64 / secs,
                stats::WAKE_FD.load(Relaxed),
                stats::WAKE_NOTIFY.load(Relaxed),
                stats::WAKE_TIMER.load(Relaxed),
            );
            println!(
                "  pkts: tx={} ({:.0}/s) rx={} ({:.0}/s), fsm_in={} MiB",
                stats::TX_PKTS.load(Relaxed),
                stats::TX_PKTS.load(Relaxed) as f64 / secs,
                stats::RX_PKTS.load(Relaxed),
                stats::RX_PKTS.load(Relaxed) as f64 / secs,
                stats::FSM_BYTES_IN.load(Relaxed) / (1024 * 1024),
            );
        }
    }));
}

// ── Mock proxy of the host ──────────────────────────────

/// The direction that the mock tests.
#[derive(Clone, Copy)]
enum Mode {
    /// Send `TOTAL` bytes to the guest sink and then close. The sends do not
    /// wait for replies, as in `RpcTransport::poll_write` of the CLI.
    Download,
    /// Count and discard the guest bytes. When the guest closes its side,
    /// close the host side, so that the client gets a clean EOF.
    Upload,
}

/// Mock `NetworkProxy` of the host for one direction.
struct MockProxy(Mode);

impl network_proxy::Server for MockProxy {
    async fn connect(
        self: Rc<Self>,
        params: network_proxy::ConnectParams,
        mut results: network_proxy::ConnectResults,
    ) -> Result<(), capnp::Error> {
        let client = params.get()?.get_client()?;
        match self.0 {
            Mode::Download => {
                tokio::task::spawn_local(async move {
                    let buf = vec![0xA5u8; CHUNK];
                    let mut sent = 0usize;
                    while sent < TOTAL {
                        let n = CHUNK.min(TOTAL - sent);
                        let mut req = client.send_request();
                        req.get().set_data(&buf[..n]);
                        drop(req.send());
                        sent += n;
                        // Let the RPC connection send its queue between
                        // chunks. It runs on the same thread, but the real
                        // sender is in a different process.
                        tokio::task::yield_now().await;
                    }
                    let _ = client.close_request().send().promise.await;
                });
                results
                    .get()
                    .init_result()
                    .set_server(capnp_rpc::new_client(DiscardSink));
            }
            Mode::Upload => {
                results
                    .get()
                    .init_result()
                    .set_server(capnp_rpc::new_client(CountingSink {
                        client,
                        received: Cell::new(0),
                    }));
            }
        }
        Ok(())
    }
}

/// Sink from the guest to the host that accepts all data and discards it.
struct DiscardSink;

impl tcp_sink::Server for DiscardSink {
    async fn send(self: Rc<Self>, _params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        Ok(())
    }
}

/// Sink from the guest to the host for the upload benchmark. It counts the
/// bytes. When the guest closes, it closes the host side, so that the last
/// `read` of the client returns EOF.
struct CountingSink {
    client: tcp_sink::Client,
    received: Cell<u64>,
}

impl tcp_sink::Server for CountingSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        let len = params.get()?.get_data()?.len() as u64;
        self.received.set(self.received.get() + len);
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        assert_eq!(self.received.get(), TOTAL as u64, "upload truncated");
        let _ = self.client.close_request().send().promise.await;
        Ok(())
    }
}

/// Connect a `MockProxy` through a real two-party RPC connection, as the CLI
/// does in production. A tokio duplex stream is used in place of the vsock.
fn start_mock_proxy(mode: Mode) -> network_proxy::Client {
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);

    let (sr, sw) = tokio::io::split(server_stream);
    let server_network = twoparty::VatNetwork::new(
        sr.compat(),
        sw.compat_write(),
        rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    let server: network_proxy::Client = capnp_rpc::new_client(MockProxy(mode));
    let rpc_system = RpcSystem::new(Box::new(server_network), Some(server.client));
    tokio::task::spawn_local(rpc_system);

    let (cr, cw) = tokio::io::split(client_stream);
    let client_network = twoparty::VatNetwork::new(
        cr.compat(),
        cw.compat_write(),
        rpc_twoparty_capnp::Side::Client,
        capnp::message::ReaderOptions::default(),
    );
    let mut rpc_system = RpcSystem::new(Box::new(client_network), None);
    let proxy = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc_system);

    proxy
}

// ── Interface setup with ioctl ──────────────────────────
//
// The benchmark environment has no /sbin/ip, which the production setup
// runs. Thus the address, netmask, UP flag and route are set with the
// SIOC* ioctls.

const SIOCSIFADDR: libc::Ioctl = 0x8916;
const SIOCSIFNETMASK: libc::Ioctl = 0x891c;
const SIOCGIFFLAGS: libc::Ioctl = 0x8913;
const SIOCSIFFLAGS: libc::Ioctl = 0x8914;
const SIOCADDRT: libc::Ioctl = 0x890B;

/// `struct ifreq` with an address, for SIOCSIFADDR and SIOCSIFNETMASK.
#[repr(C)]
struct IfreqAddr {
    name: [u8; libc::IFNAMSIZ],
    addr: libc::sockaddr_in,
    _pad: [u8; 8],
}

/// `struct ifreq` with flags, for SIOCGIFFLAGS and SIOCSIFFLAGS.
#[repr(C)]
struct IfreqFlags {
    name: [u8; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

/// Route entry for SIOCADDRT. The field names are the same as in the kernel
/// `struct rtentry` (net/route.h).
#[repr(C)]
#[allow(clippy::struct_field_names)]
struct RtEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_pad4: *mut libc::c_void,
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

/// Make an IPv4 socket address with port 0.
fn sockaddr_in(ip: [u8; 4]) -> libc::sockaddr_in {
    #[allow(clippy::cast_possible_truncation)]
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_addr.s_addr = u32::from_ne_bytes(ip); // Network order is the byte order of `ip`.
    sa
}

/// Convert an IPv4 socket address to the generic `sockaddr`.
fn as_sockaddr(sa: libc::sockaddr_in) -> libc::sockaddr {
    unsafe { std::mem::transmute(sa) }
}

/// Make the fixed-size interface name for an `ifreq`.
fn ifname(name: &str) -> [u8; libc::IFNAMSIZ] {
    let mut buf = [0u8; libc::IFNAMSIZ];
    buf[..name.len()].copy_from_slice(name.as_bytes());
    buf
}

/// Panic with `what` and the OS error if `ret` is negative.
fn check(what: &str, ret: libc::c_int) {
    assert!(ret >= 0, "{what}: {}", std::io::Error::last_os_error());
}

/// Set the address and netmask of the TUN, set it UP, and add a route to
/// the subnet of the mock server.
fn setup_iface(net: &BenchNet) {
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        check("socket", sock);

        let mut req = IfreqAddr {
            name: ifname(net.tun_name),
            addr: sockaddr_in(net.tun_ip),
            _pad: [0; 8],
        };
        check("SIOCSIFADDR", libc::ioctl(sock, SIOCSIFADDR, &raw mut req));
        req.addr = sockaddr_in(NETMASK);
        check(
            "SIOCSIFNETMASK",
            libc::ioctl(sock, SIOCSIFNETMASK, &raw mut req),
        );

        let mut fl = IfreqFlags {
            name: ifname(net.tun_name),
            flags: 0,
            _pad: [0; 22],
        };
        check("SIOCGIFFLAGS", libc::ioctl(sock, SIOCGIFFLAGS, &raw mut fl));
        fl.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        check("SIOCSIFFLAGS", libc::ioctl(sock, SIOCSIFFLAGS, &raw mut fl));

        let mut dev = ifname(net.tun_name);
        let mut rt: RtEntry = std::mem::zeroed();
        rt.rt_dst = as_sockaddr(sockaddr_in(net.dst_net));
        rt.rt_gateway = as_sockaddr(sockaddr_in([0, 0, 0, 0]));
        rt.rt_genmask = as_sockaddr(sockaddr_in(NETMASK));
        rt.rt_flags = libc::RTF_UP;
        rt.rt_dev = dev.as_mut_ptr().cast();
        check("SIOCADDRT", libc::ioctl(sock, SIOCADDRT, &raw mut rt));

        libc::close(sock);
    }
}
