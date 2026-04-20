# Network passthrough for non-HTTP protocols

## Symptom

A `psql` running inside the sandbox and targeting a host-side Postgres via
a port-forward (`localhost:40000` → host `5432`) hung indefinitely after
`connect`. Airlock logs showed the connect succeeding and nothing after.

## Root cause

The host-side network proxy always runs two peeks on the container
stream before it decides how to relay: a TLS `ClientHello` sniff, then
an HTTP request-line / H2 preface sniff. The HTTP detector in
`app/airlock-cli/src/network/http.rs` reads until it sees `\r\n` or
4096 bytes, whichever comes first.

Postgres opens with an 8-byte `SSLRequest` (`00 00 00 08 04 d2 16 2f`):
no `\r\n`, no further bytes until the server responds. The server-side
TCP socket isn't opened until *after* detection completes — so the
container writes 8 bytes, the detector waits for more, and the server
end sits unopened waiting for the detector to hand off. Deadlock.

## Fix

A new per-rule `passthrough: bool` opt-in short-circuits all
interception: the connection is opened to the real server immediately
and relayed byte-for-byte.

```toml
[network.rules.database]
allow = ["db.example.com:5432"]
passthrough = true
```

Changes:

1. `NetworkRule.passthrough` added to the config (default false).
2. `rules::resolve` returns a `RuleTargets { allow, deny, passthrough }`
   struct; `passthrough` is a subset of `allow` whose rule had the
   flag set.
3. `Network` carries `passthrough_targets: Vec<NetworkTarget>`.
   `resolve_target` sets `ResolvedTarget.passthrough` when the
   host:port matches any of them, and unconditionally for
   port-forwarded destinations (a guest `localhost:<port>` can be
   talking to anything on the host side — interception is unsafe by
   default).
4. `ResolvedTarget::is_passthrough()` method (reintroduced — it
   previously existed but was removed when MITM became
   unconditional; now it's config-driven rather than derived from
   "no middleware attached").
5. `handle_connection` short-circuits before `tls::detect` when
   `is_passthrough()`: it wires `container_transport` to
   `tcp::connect_server` and calls `tcp::relay` directly. No TLS
   sniff, no HTTP detect, no middleware.
6. Startup-time conflict check (`check_passthrough_conflicts` in
   `network/check_target_conflicts.rs`) rejects configs where any
   enabled passthrough target overlaps any enabled middleware target,
   naming every offending pair. Overlap is a grammar-aware
   intersection over the narrow pattern language (`*`, `*.<suffix>`,
   exact literal, localhost aliases) — same semantics as the runtime
   `matchers::host_matches`. Wildcards are RFC 6125-strict: a
   `*.<suffix>` wildcard matches exactly one leading label (no apex,
   no multi-label prefix), so two wildcards can only overlap when
   their suffixes are identical, and `*.example.com` no longer
   overlaps `example.com` or `a.b.example.com`. A literal × wildcard
   overlap is just "does the literal match the wildcard."
   `matchers::host_matches` was tightened at the same time — it
   previously accepted both the apex and multi-label prefixes, which
   contradicted RFC 6125 and silently over-matched at connect time.

Unix socket forwards don't need a flag: they already bypass TLS/HTTP
detection via `spawn_socket_connection`, which goes straight to
`tcp::relay`.

## Why not short-circuit inside `http::detect`

First thought was to detect "the first byte can't start an HTTP method"
and give up early. Rejected: we *want* to intercept allowed TLS/HTTP
traffic by default — that's what makes the Monitor's Requests sub-tab
useful. Heuristics there would quietly break interception for borderline
bytes. Explicit `passthrough = true` in config makes the tradeoff
visible to the user and keeps the default path MITM-everywhere.

## Why not a global flag

One project can legitimately want to proxy `api.example.com` with
middleware *and* relay `db.example.com:5432` as raw TCP. Per-rule is
the right granularity. Conflict detection catches the accidental
overlap.
