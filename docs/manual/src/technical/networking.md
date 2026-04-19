# Networking

All outbound network access from the container goes through a
host-side proxy. There is no route from the VM to the outside world
except via the supervisor's vsock RPC — the kernel is built without a
real egress NIC, and iptables inside the VM redirects every outbound
TCP connection to an in-VM proxy listener that relays through RPC.

```
container socket()/connect(host, port)
  → virtual DNS maps host → synthetic IP in 10.2.0.0/16
  → iptables TCP REDIRECT to 127.0.0.1:15001 (in-VM proxy)
  → SO_ORIGINAL_DST recovers the synthetic IP, reverses → host
  → supervisor.NetworkProxy.connect(host:port) over vsock
  → CLI on host resolves host, dials real server (+ optional TLS MITM)
  → bidirectional byte relay
```

## Virtual DNS

The container's `/etc/resolv.conf` points at `nameserver 10.0.0.1`,
which is a minimal UDP DNS server the supervisor runs inside the VM.
Instead of forwarding DNS queries to the host, the supervisor
allocates a **synthetic IP** from `10.2.0.0/16` for each hostname that
gets queried and caches the bidirectional mapping.

This matters because the proxy sees an IP, not a name. When the
container `connect()`s to the synthetic IP, iptables `REDIRECT`
rewrites the destination to `127.0.0.1:15001` but preserves the
original in the socket's `SO_ORIGINAL_DST`. The proxy recovers the
synthetic IP, reverse-looks it up in the DNS cache, and has the real
hostname for policy evaluation and TLS SNI. This works uniformly for
HTTP, HTTPS, and raw TCP — there is no protocol-specific logic.

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
in-VM proxy is a pure TCP relay: `SO_ORIGINAL_DST` + DNS reverse
lookup + raw byte forwarding over vsock. The CLI then:

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

Ports declared as "host ports" in the config get per-port iptables
`REDIRECT` rules inside the VM so that connections to
`127.0.0.1:<port>` are transparently forwarded to the host. Other
localhost traffic passes through directly so local VM services still
work. The redirected traffic lands on the in-VM proxy listener, which
opens a new connection back to the host port via the `NetworkProxy`
RPC.

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
