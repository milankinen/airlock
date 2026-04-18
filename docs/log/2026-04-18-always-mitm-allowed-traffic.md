# Always MITM allowed TLS/HTTP traffic

## What

Removed the TLS passthrough code path. Every allowed connection now goes
through the same pipeline: TLS detect → (if TLS) MITM via the per-project
CA → HTTP detect → relay (HTTP or raw TCP). Whether a middleware script
matches only decides what the HTTP relay does to the request — not
whether the bytes are decrypted.

## Why

The Monitor tab's Requests sub-tab depends on the HTTP relay emitting
`NetworkEvent::Request` for each request it sees. Under the old
`is_passthrough` rule — "allowed && no matching middleware" — the vast
majority of connections skipped MITM entirely, so the Requests log was
effectively empty unless the user had middleware configured for every
host they cared about.

Observing traffic is the whole point of a sandbox's network tab. Making
that observation conditional on middleware being present coupled two
independent features: "decrypt so we can see it" and "transform it via
Lua." The user shouldn't have to write a no-op middleware rule just to
see what requests their sandbox is making.

The trade-off is that the CA is now used for every allowed host rather
than only hosts explicitly targeted by middleware. The CA is already
per-project and lives only inside the VM's trust store, so this doesn't
widen the blast radius of the CA itself — it just means more
connections are re-signed by it. Container-side processes already trust
it, so nothing breaks.

## How

- `crates/airlock/src/network/target.rs`: deleted the
  `ResolvedTarget::is_passthrough` method.
- `crates/airlock/src/network/server.rs`: dropped the early-return
  passthrough branch in `handle_connection`. Updated the doc comment.
- `crates/airlock/src/network/tests/helpers.rs`: removed the
  `tls_passthrough` field from `TestNetworkConfig` and the
  `run_network*` signatures. Also removed the synthetic "no-op
  middleware forces MITM" workaround — it existed purely to opt tests
  out of the old passthrough default, which is gone.
- `crates/airlock/src/network/tests/test_tls.rs`: deleted the
  `tls_passthrough` test. The concept no longer exists; the `tls_mitm_*`
  and ALPN tests continue to exercise the MITM path that's now the only
  path.
- `crates/airlock/src/network/tests/{test_tcp,test_http,test_middleware}.rs`:
  updated call sites for the new `run_network*` arity.

## Trade-offs / what's deliberately not done

- **No opt-out.** The user can still choose not to allow a host at all;
  that's the escape hatch. We deliberately don't expose "allow but
  don't intercept" as a third mode — its only legitimate use case
  (pinned-cert clients that refuse the project CA) is narrow enough to
  revisit only if a real user hits it.
- **No change to non-TLS traffic.** Plain TCP and plain HTTP already
  went through the regular relay; nothing there needed to move.

## Files

- `crates/airlock/src/network/target.rs`
- `crates/airlock/src/network/server.rs`
- `crates/airlock/src/network/tests/helpers.rs`
- `crates/airlock/src/network/tests/test_tls.rs`
- `crates/airlock/src/network/tests/test_tcp.rs`
- `crates/airlock/src/network/tests/test_http.rs`
- `crates/airlock/src/network/tests/test_middleware.rs`
- `docs/manual/src/advanced/network-scripting.md`
