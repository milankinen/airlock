# TCP proxy on a TUN device

Replace the iptables-REDIRECT + `127.0.0.1:15001` proxy design with
a smoltcp-userspace-TCP stack running on a TUN device (`airlock0`)
that sits at the VM's default route. Every outbound TCP flow — from
the VM's root netns, from daemon sidecars, and crucially from
container network namespaces — lands in the proxy and is relayed to
the host over the same `NetworkProxy.connect` RPC the old path used.

## Why

iptables `-t nat -A OUTPUT -j REDIRECT --to-port 15001` only fires
for locally-originated packets. Traffic forwarded from a container
netns traverses PREROUTING, not OUTPUT, so it never matched the rule.
Users ran into this immediately with Docker: bridge-network containers
could not reach `pypi.org`, Docker Hub, etc. The earlier workaround
was `network_mode: host`, which loses Compose's service DNS and
collapses every container into the VM's port space.

A TUN at the default route solves the root cause: the kernel delivers
every egress packet to the TUN's fd regardless of which netns emitted
it. No chain-matching subtleties.

## Design

### Packet interception

`net::tcp_proxy` opens `/dev/net/tun` (creating the node via `mknod`
if devtmpfs didn't populate `/dev/net/`), assigns the interface
`192.168.77.1/24`, and installs `ip route add default dev airlock0`.
The kernel then routes every non-loopback TCP packet to the TUN fd.
An `AsyncFd` wakes the supervisor when a packet arrives; `drain_rx`
pulls packets into a queue consumed by smoltcp via the `Device` trait.

### Dynamic listeners via SYN-snooping

smoltcp's `tcp::Socket::listen` requires a specific `(addr, port)` up
front; it has no "listen on any port" mode. We inspect every rx
packet with `Ipv4Packet::new_checked` + `TcpPacket::new_checked`, and
for SYNs matching a new `(src, dst)` pair we call
`tcp::Socket::listen(IpListenEndpoint { addr: Some(dst.ip()), port: dst.port() })`
*before* smoltcp processes the SYN. This pattern (borrowed from
microsandbox's `crates/network/lib/stack.rs`) gives any-port
interception without needing a raw socket.

`set_any_ip(true)` plus a smoltcp-internal route
`0.0.0.0/0 via 192.168.77.1` lets smoltcp accept packets destined to
IPs it doesn't own — required because destinations are arbitrary
(virtual-DNS synthetic IPs in `10.2.0.0/16`, container MASQUERADE'd
packets, etc.).

### Split-poll APIs (smoltcp 0.13)

Upgraded from smoltcp 0.11 to 0.13 to use `poll_ingress_single` /
`poll_egress` / `poll_maintenance` instead of the monolithic `poll()`.
This gives tight ordering: whenever an ingress packet causes a state
change (ESTABLISHED on 3WHS completion), we immediately run the FSM
so the per-connection relay agent gets spawned before the next packet
is processed.

### Per-connection relay

On first ESTABLISHED, the FSM spawns a `relay_agent` task that opens
`NetworkProxy.connect(host, port)` over vsock. The hostname is
recovered from the virtual-DNS reverse map when the destination is a
synthetic `10.2.0.X` IP. Bytes flow:

- smoltcp recv buffer → `to_host` mpsc channel (drained by poll loop)
- `to_host` channel → `client_sink.send()` (pumped by relay agent)
- Host → `ChannelSink::send` → `from_host` mpsc channel (pushed by
  Cap'n Proto runtime)
- `from_host` channel → `sock.send_slice()` (pumped by poll loop,
  with partial-write leftovers parked in `pending_tx`)

Half-closes propagate in both directions: guest FIN → drop
`to_host` sender → agent sees EOF, calls `client_sink.close()` →
host side shuts down. Host close → agent's `from_host_tx` drops →
poll loop sees `Disconnected` → `sock.close()` sends FIN to guest.

### `Notify`-based wakeup

Host → guest bytes arrive via a userspace mpsc channel that can't
make a kernel fd readable. Without help, the poll loop would only
wake on TUN-fd readiness or a timer, adding up to the fallback
sleep (100ms) of latency to every host-originated byte. A shared
`Rc<Notify>` fixes this: every `ChannelSink::send` / `close` pings
the notify, and the poll loop `select!`s on `notify.notified()`
alongside `async_fd.readable()` and a `sleep(100ms)` safety net.
Host bytes wake the poll loop within scheduler latency rather than
idle-tick latency.

### Per-port loopback listeners for host-published ports

Host-published ports (`guest = [...]` in config, and the legacy
host-port feature) used to rely on iptables REDIRECT rules steering
`127.0.0.1:<port>` traffic to the shared 15001 proxy, which used
`SO_ORIGINAL_DST` to recover the target. Now `net::host_port_forward`
binds a dedicated `TcpListener` per port during setup, before any
daemon or user process starts — so the supervisor wins the bind
race. Each listener knows its port; no REDIRECT, no
`SO_ORIGINAL_DST`, no shared proxy. On accept it calls
`rpc_connect_tcp("127.0.0.1", port)` and relays bytes with the
shared `relay` helper.

### Module shape

- `net::tcp_proxy` — the TUN + smoltcp + relay agents.
- `net::rpc_bridge` — shared `ChannelSink`, `relay`, `rpc_connect_tcp`,
  and `open_local_tcp` (used by the `Supervisor.openLocalTcp` RPC
  handler for the reverse host → guest TCP path).
- `net::host_port_forward` — per-port loopback listeners (renamed
  from `net::host_ports`).
- `net::host_socket_forward` — unix socket forwarding (renamed from
  `net::socket` for symmetry).
- `net::proxy` (the old 15001 accept loop + `get_original_dst`) is
  deleted.

### `init::linux::net` simplification

The whole iptables dance is gone. What remains is: `lo up`,
`10.0.0.1/32 on lo` (for the in-VM virtual DNS server),
`rp_filter=0` and `ip_forward=1` sysctls, and `route_localnet` on lo.
The `/32` is important — the old code used `10.0.0.1/8` which
shadowed the synthetic `10.2.0.0/16` route back onto loopback once
iptables was removed, leaving every outbound connection to a
virtual IP dying on a loopback address with no listener.

## Trade-offs considered

### Fixed-port listeners vs SYN-snooping

Earlier iterations used a pool of fixed-port wildcard listeners
(smoltcp has no "any-port" listen) — good enough for a PoC, useless
in practice because we can't predict which ports containers will
connect to. SYN-snooping adds one pre-poll parsing pass per packet
and one socket allocation per flow; on the scale of sandbox
workloads this is free.

### Single-threaded vs multi-threaded poll

Considered pushing smoltcp onto a dedicated OS thread to isolate
packet processing from RPC/DNS/daemon supervision. Rejected because
`network_proxy::Client` (the Cap'n Proto RPC client) is `!Send`;
crossing threads means every RPC call becomes a cross-thread
channel round-trip. smoltcp is explicitly designed for
single-threaded sync operation (no locks, everything on the task's
stack). Revisit only if profiling shows contention.

### MASQUERADE source rewriting

Docker's POSTROUTING MASQUERADE rewrites container src IPs to
airlock0's IP before the packet hits the proxy, so we can't
distinguish "container A" from "container B" from "root-netns
process" at smoltcp. Considered adding a RETURN rule to bypass
MASQUERADE for the TUN range, or using conntrack to recover the
pre-NAT source. Both rejected: airlock's trust boundary is the VM,
not the container — everything inside is one tenant, policy keys
on destination, not source. Let docker do its thing.

### smoltcp 0.11 → 0.13

0.11's monolithic `poll()` works, but the 0.13 split APIs give us
per-packet ordering (spawn the agent before the next packet) and
explicit egress/maintenance separation. API break cost was small
(`Ipv4Address` became a re-export of `std::net::Ipv4Addr`,
`RxToken::consume` takes `&[u8]` instead of `&mut [u8]`).

## Ancillary changes

- `CONFIG_TUN=y` added to the arm64 kernel config — x86_64 already
  had it. Without this the ioctl returns `ENODEV`.
- `/dev/net/tun` mknod fallback in `net::tun::ensure_tun_dev`:
  devtmpfs doesn't always populate `/dev/net/` subdirectory without
  udev, so we create the character device ourselves if missing.
- Docker example rewritten to use bridge networking + PEP 723
  inline script deps (uv) + `ports:` publishing. Drops
  `network_mode: host`, the `app.dockerfile` build step, and the
  `--build` flag in the README. `SSL_CERT_FILE` bind-mounts the VM's
  CA bundle into the container so uv trusts the airlock TLS MITM.
- `dev.dockerfile`: added `iproute2 tcpdump iptables` for debugging
  inside the dev container.
- Logging cleaned up: per-connection events at `debug`, startup and
  terminal I/O failures at `info` / `error`. Dropped the chatty
  `smoltcp poll task: entered` diagnostic and the per-packet
  `tun rx:` logs.

## Verification

- `mise lint` + `mise run test` pass.
- Inside the VM, `curl` from both root netns and from a Docker
  container reaches external destinations with `tcp-proxy accept`
  logs appearing for each flow.
- `docker compose up` with the rewritten example runs end-to-end:
  `uv run` installs Flask + valkey over MITM'd HTTPS (via bundled
  CA), the app container resolves `valkey` via Compose DNS, and
  `http://localhost:8000` on the host serves the hit counter.
