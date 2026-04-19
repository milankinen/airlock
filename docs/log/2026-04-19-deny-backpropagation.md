# Deny-request back-propagation to the VM

Inside the sandbox, a blocked network request looks like an ordinary
connection failure: `ECONNREFUSED`, a TLS handshake reset, a 403 from
the proxy. That ambiguity makes it hard for tools inside the VM to give
users a useful error — "your build failed because `registry.npmjs.org`
isn't on the allow list" reads very differently from "fetch: network
error."

## Notification path

Added `Supervisor.reportDeny(epoch)` to the capnp schema. The host is
already the RPC client (the guest hosts the `Supervisor` interface), so
this is a natural one-way call: every deny site on the host fires the
RPC fire-and-forget, the guest records the timestamp.

Four deny sites in `airlock-cli`:

1. `server.rs` — socket deny under `deny-always` policy.
2. `server.rs` — socket deny when no matching socket rule.
3. `http.rs` — HTTP 403 when `!target.allowed`.
4. `http.rs` — non-HTTP relay when `!target.allowed` (null server sink).
5. `middleware.rs` — Lua script called `req:deny()`.

All five call `deny_reporter.report()`. The reporter is a small
`Rc<DenyReporter>` threaded into `Network` at construction with a
`RefCell<Option<supervisor::Client>>` inside. The client is attached
in `cmd_start.rs` right after the vsock handshake; before that attach
(including in unit tests with a mocked `Network`), `report()` is a
silent no-op. Fire-and-forget via `spawn_local` so the deny path
doesn't wait on an RPC round-trip.

## Guest endpoint

`airlockd` runs an axum server on `0.0.0.0:1337` serving a single
`GET /last_deny` route. The shared state is `Arc<DenyTracker>` where
`DenyTracker` wraps an `AtomicU64` — `0` is the sentinel for "no deny
yet" (Unix epoch 0 isn't realistic). Axum requires `Send + Sync` state
even on a current-thread runtime, which is why we use `Arc + Atomic`
instead of `Rc + Cell` like the surrounding supervisor state.

Response format: decimal epoch seconds followed by `\n`, or an empty
body when no deny has been reported. The simplest thing that works —
tools can `[[ -n "$(curl -s …)" ]]` for "any deny" or diff timestamps
for "new deny since I started."

## Why axum (and not hand-rolled)

Started with a 40-line hand-rolled HTTP/1.1 handler to avoid pulling
hyper into the guest binary. Switched to axum on user preference: the
workspace already depends on axum 0.8 for the host side, the binary
size delta is negligible compared to the existing hyper/tokio/capnp
footprint, and it saves us from maintaining a toy HTTP parser. axum
also cleanly supports the shared-state model we want.

## Why not batch

One RPC per deny. If a buggy script hammers a denied endpoint in a
tight loop, that's one small capnp call per request — still cheap
compared to the TLS handshake and HTTP parsing that precedes it, and
the guest just overwrites the atomic. If it becomes a hotspot we can
debounce on the host side, but for now "one RPC per deny" keeps the
code trivial.

## Scope of "deny"

Only user-visible denies are reported. Specifically, we do *not* fire
when a TCP relay's null-sink silently drops bytes on an allowed-but-
failed upstream (those are upstream errors, not policy denies). The
five sites above correspond exactly to the five places where airlock's
policy engine rejects something the container asked for.
