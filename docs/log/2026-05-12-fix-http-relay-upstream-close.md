# Fix HTTP relay upstream close

## Problem

When running copilot-cli inside an airlock sandbox, the first prompt
succeeds but all subsequent prompts fail with 502 "operation was
canceled" errors.

## Root cause

The HTTP relay in `http::relay()` creates a single upstream hyper
sender (H1 or H2) per guest TCP connection. The upstream connection
handle is spawned as a fire-and-forget background task. When the
upstream server closes the connection (keep-alive timeout, GOAWAY,
idle timeout), that background task completes and the sender goes
stale — but nobody notices. Every subsequent request from the guest on
the **same** TCP connection calls `sender.send()` on the dead sender,
which returns hyper's "operation was canceled" error. The relay wraps
this in a 502 response.

The typical timeline:
1. Guest opens TCP connection → TLS MITM → relay creates sender
2. First request succeeds (sender is alive)
3. ~30s idle → upstream server drops connection → sender stale
4. Guest sends second request → sender.send() fails → 502
5. Guest retries on same connection → 502 forever

Existing tests never caught this because the `http_get()` test helper
always sets `Connection: close`, so each test uses exactly one request
per connection.

## Fix: mirror upstream state

Instead of spawning the upstream connection as fire-and-forget, keep
the `JoinHandle`. Use `tokio::select!` to race the guest-side
`serve_connection` against the upstream conn handle. When the upstream
closes first, call `connection.as_mut().graceful_shutdown()` on the
guest side. This sends H2 GOAWAY or stops accepting new H1 requests,
then awaits completion of any in-flight request before closing the
guest connection cleanly. The guest sees a normal connection close and
reconnects naturally — creating a fresh relay with a new sender.

Key implementation details:
- `serve_connection` returns a future that must be pinned. The
  `Builder` is a temporary, so it must be bound to a `let` to satisfy
  the borrow checker before calling `.serve_connection()`.
- `graceful_shutdown()` takes `Pin<&mut Self>`, so we use
  `connection.as_mut().graceful_shutdown()` on the pinned future.

## Tests added

- `http_keepalive_multiple_requests`: sends two requests on a single
  keep-alive connection. Verifies basic keep-alive works.
- `http_upstream_close_propagates_to_guest`: sends a request, shuts
  down the upstream server, then verifies the guest connection closes
  cleanly (no 502).

Added `http_get_keepalive()` helper (GET without `Connection: close`)
and `serve_with_shutdown()` helper (returns a oneshot sender to trigger
graceful server shutdown on demand).
