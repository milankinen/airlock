# Reverse port forwards (host → guest)

Until now `[network.ports.<name>].host = [...]` could only forward in
one direction: a guest process connecting to `localhost:<port>`
reached a service running on the host. There was no way for a host
process to reach a service running *inside* the sandbox.

This change adds the reverse direction as a sibling field
`[network.ports.<name>].guest = [...]` with the same `"host:guest"`
string syntax as `.host`, plus a plain integer shorthand. The left
side is always the host port; the right side is always the guest
port. That convention is the same regardless of which list an entry
sits under.

## Direction contract

| Field | Entry         | Listens on                  | Forwards to            |
|-------|---------------|-----------------------------|------------------------|
| host  | `"9000:3000"` | guest-side `127.0.0.1:3000` | host `127.0.0.1:9000`  |
| guest | `"5000:4000"` | host-side  `127.0.0.1:5000` | guest `127.0.0.1:4000` |

## `PortMapping` field rename

`PortMapping { source, target }` became `PortMapping { host, guest }`.
The `"a:b"` parser is unchanged — only the in-code field names moved
so they stay unambiguous whether a mapping is under `.host` or
`.guest`. Serialize still emits the plain-int short form when
`host == guest`.

## Schema — `openLocalTcp` on `Supervisor`

A new guest-served RPC method:

```capnp
openLocalTcp @5 (port :UInt16, client :TcpSink) -> (server :TcpSink);
```

Symmetric to `NetworkProxy.connect` but on the guest side. The host
calls it once per accepted TCP connection; the guest opens
`127.0.0.1:<port>` and the two sinks carry raw bytes in each
direction. No `denied` arm — the host is trusted; connect failures
inside the guest surface as Cap'n Proto exceptions so the host can
close the accepted socket.

## Why reverse forwards bypass the rules engine

The whole rules/policy/middleware stack exists to constrain traffic
that originates from *untrusted* guest code reaching out to the
internet. Reverse forwards are the inverse: trusted host code reaching
into a local guest service.

Running those connections through the middleware pipeline would mean:
matching them against rules that were written with guest→external
semantics, invoking TLS interception for loopback traffic, and giving
`deny-always` the power to break tools the user is actively trying to
use against the sandbox (e.g. port-forwarding a dev server to their
editor). None of that is useful here.

So host → guest is a raw TCP relay with no detection and no policy
coupling. `deny-always` intentionally does not block it.

## Bind-time guarantees

- **Loopback only.** Listeners bind on `127.0.0.1`. LAN exposure is
  out of scope — users who need it can chain `socat` or similar.
- **Hard fail on bind error.** If `bind()` returns `EADDRINUSE` (or
  any other error), sandbox startup aborts with a clear message. No
  silent degradation to a half-working sandbox.
- **Startup-time conflict check.** Two `.guest` entries sharing the
  same host port is rejected with both labels named, matching the
  existing passthrough/middleware conflict reporting.

Cross-direction conflicts are intentionally not flagged: a host port
listed in both `.host = [5000]` and `.guest = [5000]` is legal
because `.host` never binds anything on the host side — it only
influences guest-side remap decisions. Only `.guest` entries bind
host listeners, so only `.guest`-vs-`.guest` duplicates can collide.

## Implementation shape

- `app/airlock-cli/src/network/reverse_forward.rs` — new module split
  into `bind()` and `serve()`. `bind()` runs before the VM boots:
  every listener is opened eagerly so `EADDRINUSE` (or any other
  bind failure) aborts startup *before* the VM is spawned, rather
  than after the sandbox is already running. `serve()` runs once the
  supervisor RPC client is available and attaches each bound
  listener to a per-listener accept loop. Each accepted connection
  calls `openLocalTcp` and bridges via the existing `tcp::relay` +
  `RpcTransport` machinery used by the forward direction.
- `app/airlockd/src/rpc.rs` — `SupervisorImpl::open_local_tcp`
  dispatches to a helper in `net/proxy.rs` that connects inside the
  guest and wires the sink pair to the local TcpStream using the
  same relay pattern as the guest-originated proxy path.

No new transport code — both sides reuse the TcpSink/ChannelSink/relay
primitives that were already in place for the forward direction.
