# Honor half-close in the loopback and unix-socket relays

## Symptom

A guest client talking to a host-published port or a forwarded unix socket
could lose the server's reply. For a request→half-close→await-reply protocol
(redis-style, or an RPC that `shutdown(SHUT_WR)`s after sending), the client
sent its request, half-closed its write side, and waited for the response — but
the relay tore the whole connection down at that point and the response was
truncated or lost.

## Root cause

Both bidirectional relays (`net/rpc_bridge.rs::relay` and
`net/host_socket_forward.rs`) used `tokio::select!` over the two copy loops:
whichever direction finished first cancelled the other and then closed both
the remote sink and the local write half. A one-way EOF (the client
half-closing its send side) therefore killed the still-active
response direction.

Note: `net/tcp_proxy.rs` already handles half-close correctly with independent
per-direction state — only these two loopback/unix relays were affected.

## Fix

Each direction now runs to completion independently via `tokio::join!`, and
each performs only its own half-close:

- when the local→remote copy ends, we send the remote sink a `close` (EOF for
  that direction) but keep draining the remote→local response;
- when the remote→local copy ends, we shut down the local write half.

So a one-way EOF only half-closes that direction; the connection stays open
until both directions are actually done, which is correct TCP half-close
behavior and no longer truncates replies.

## Tests

No isolated unit test was added: both relays are driven by capnp `tcp_sink`
RPC clients and mpsc channels that can't be exercised without a running guest
transport. The behavior is covered by the VM bats suite (`bats:vm`, needs
KVM + Docker), not run here. The change is a localized control-flow fix
(`select!` → `join!` with per-direction half-close).
