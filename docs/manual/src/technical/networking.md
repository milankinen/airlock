# Networking

All outbound network access from the VM goes through a host-side
proxy. There is no route from the VM to the outside world except via
the supervisor's vsock RPC — the kernel is built without a real
egress NIC, and every outbound TCP connection is intercepted by a
userspace TCP/IP stack (smoltcp) running on an in-VM TUN device
(`airlock0`). The TUN is wired as the VM's default route, so every
packet that isn't loopback-local ends up in the proxy regardless of
which netns it originated in (including Docker containers).

```
container socket()/connect(host, port)
  → virtual DNS maps host → synthetic IP in 10.2.0.0/16
  → default route sends packet to airlock0 (TUN device)
  → smoltcp parses the SYN, creates a listener for (dst_ip, dst_port)
  → TCP handshake completes inside smoltcp
  → DNS reverse-lookup maps synthetic IP back to hostname
  → supervisor.NetworkProxy.connect(host:port) over vsock
  → CLI on host resolves host, dials real server (+ optional TLS MITM)
  → bidirectional byte relay (smoltcp socket ↔ RPC sinks)
```

The TUN-based approach replaced an earlier iptables `REDIRECT` design.
The fatal flaw of iptables REDIRECT is that the OUTPUT chain only
fires for *locally originated* traffic. Packets forwarded from a
container netns traverse PREROUTING, not OUTPUT, so they never hit
the rule and never reached the proxy. A TUN at the default route
catches everything — no chain-matching subtleties.

## Virtual DNS

The container's `/etc/resolv.conf` points at `nameserver 10.0.0.1`,
which is a minimal UDP DNS server the supervisor runs inside the VM
on loopback. Instead of forwarding queries to the host, the supervisor
allocates a **synthetic IP** from `10.2.0.0/16` for each hostname and
caches the bidirectional mapping.

This matters because the proxy sees an IP, not a name. When the
container `connect()`s to the synthetic IP, the packet routes via the
TUN; smoltcp snoops the SYN and creates a listener for that specific
`(dst_ip, dst_port)`. On accept, the proxy reverse-looks up the
synthetic IP in the DNS cache and has the real hostname for policy
evaluation and TLS SNI. This works uniformly for HTTP, HTTPS, and raw
TCP — there is no protocol-specific logic.

Real DNS resolution happens on the host: the synthetic IP never
escapes the VM. The CLI gets the hostname, calls the system resolver,
and connects to whatever that returns.

## Policy evaluation

Once the proxy has `(host, port)` it asks the CLI whether to allow the
connection. The policy model is:

- `allow-always` (default): skip rules, allow everything
- `deny-always`: skip rules, deny everything — including port forwards
  and socket forwarding
- `allow-by-default`: allow unless a rule explicitly denies
- `deny-by-default`: deny unless a rule explicitly allows

When a rule-based policy is in effect, the decision proceeds:

1. If any `deny` pattern matches → **block** immediately (deny wins).
2. If any `allow` pattern matches → **allow**.
3. Otherwise → follow `policy`.

Rules are additive across config files and presets; `enabled = false`
disables a rule (including one inherited from a preset).

Pattern formats (same in `allow` and `deny`):

- `host` — exact hostname, any port
- `host:port` — exact hostname and port
- `*:port` — any hostname on a specific port
- `*.suffix` — subdomain wildcard
- `*` — match all (use only for development)

## TLS interception

Per-project: a self-signed CA keypair is generated and stored in
`.airlock/sandbox/ca.json`. The CA cert PEM is passed to the guest via
the `start` RPC and injected into the rootfs by guest init — see
[Mounts / CA certificate injection](./mounts.md#ca-certificate-injection).

All TLS logic runs **in the CLI**, not in the supervisor. The
in-VM proxy is a pure TCP relay: DNS reverse lookup + raw byte
forwarding over vsock. The CLI then:

1. Incrementally reads the first bytes of the stream and uses
   `tls-parser` to validate the TLS record header. First byte `0x16`
   is not sufficient — any non-TLS stream starting with that byte
   would false-positive.
2. If a full ClientHello is recognised, the CLI terminates TLS with a
   freshly minted cert (signed by the project CA, SNI-matched) and
   opens a second TLS connection to the real server. Crucially, the
   CLI negotiates **the same ALPN** the container negotiated — so an
   HTTP/2 client talks to an HTTP/2 server, not an h1/h2 mismatch.
3. If the stream is not TLS, it's bridged as raw TCP.

This split (TCP relay in the guest, TLS in the host CLI) exists
because an earlier design did MITM inside the supervisor: the
container would negotiate h1 with the supervisor while the CLI
independently negotiated h2 with the real server, and the raw byte
relay between them would hand h1 bytes to an h2 server that rejected
them. Moving all TLS to the CLI means one endpoint owns the whole
protocol stack and ALPN can be proxied faithfully.

## Lua middleware

Middleware is a top-level `[network.middleware]` section, separate
from rules. Each entry has its own `target` patterns and a `script`.
Middleware applies to any allowed connection whose `host:port`
matches — regardless of which rule allowed it — so the same
middleware can cover traffic from multiple rules without duplication.

Scripts are compiled to bytecode at startup (zero per-request
compilation overhead) and run per HTTP request/response. See
[Network scripting](../advanced/network-scripting.md) for the
scripting API.

## Localhost port forwarding

Ports declared as "host ports" in the config get a dedicated
`TcpListener` bound by the supervisor on `127.0.0.1:<port>` before any
user process or daemon starts — so the supervisor always wins the
bind race. On accept, the listener opens a `NetworkProxy.connect`
call targeted at `127.0.0.1:<port>` and relays bytes both ways. No
iptables rules, no `SO_ORIGINAL_DST`, no shared proxy port: each
listener already knows its port.

Other localhost traffic is unaffected — it stays on `lo` and reaches
whatever is listening on the VM's loopback.

## Unix socket forwarding

Host Unix sockets are forwarded into the container. When a process in
the container connects to the guest socket path, the supervisor sends
the guest path to the CLI via `NetworkProxy.connect(socket=…)`. The
CLI maps guest path → host path using a pre-built `socket_map` (with
tilde expansion applied at setup time) and opens a connection to the
host socket.

`~` in guest paths is expanded to the container home directory (read
from the image's `/etc/passwd`). `~` in host paths is expanded to the
host user's home directory.
