# Add comprehensive network tests

33 end-to-end tests covering TCP relay, HTTP proxy, TLS MITM, TLS
passthrough, ALPN negotiation, and Lua middleware.

### Test infrastructure

- `run_network` / `run_with_config` — starts a full capnp-rpc system
  (client + server VatNetwork over tokio DuplexStream) on a LocalSet
- `TestConnection` — simulates the supervisor side, sends/receives bytes
  through the RPC channel
- `RpcStream` — `AsyncRead + AsyncWrite` adapter over the RPC channel
  for TLS client handshakes in tests
- `serve()` — axum HTTP server on random port
- `serve_https()` — HTTPS server with test CA + leaf cert
- `RequestLog` — captures Lua `log()` calls for test assertions
- `LogFn` — configurable log sink (tracing in production, collector in tests)

### Test coverage

- **TCP** (7): plain HTTP, host allowlist (deny, wildcard, star, empty),
  POST with body, large response
- **HTTP proxy** (5): detection with middleware, raw relay without
  middleware, POST through proxy, status codes, response headers
- **Middleware** (14): deny by path/host, inject headers, read/replace
  request body, JSON body coercion, explicit send + response inspection,
  modify response status/headers/body, implicit send, multiple layers,
  JSON response parsing, body length
- **TLS** (7): MITM basic, passthrough, MITM with middleware, ALPN
  h1↔h1, h1 when server offers h2+h1, no ALPN fallback, h2↔h2

### Bug fixes from tests

- `is_denied()` now checks `CallbackError` nesting (deny from Lua was
  wrapped in `CallbackError`, not matched as `ExternalError`)
- `setBody()` updates Content-Length header automatically
- Host getter strips port from Host header for `hostMatches()`
