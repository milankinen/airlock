# HTTP request interception via hyper

## Context

tcp_connect scripting works — Lua rules can allow/deny/modify at the
TCP level. Now we need http_request rules that see the full HTTP
request (method, path, headers) and can modify headers before
forwarding. This enables auth token injection, path rewriting, etc.

## Architecture

For HTTP connections, replace the raw byte relay with a proper HTTP
proxy using hyper for both parsing and forwarding:

```
Container → supervisor → RPC bytes → CLI
  ├─ Try to parse as HTTP (hyper server)
  │   ├─ Success: run http_request scripts → hyper client → real server
  │   └─ Response: stream back via RPC → supervisor → container
  └─ Not HTTP: fall back to raw byte relay (current behavior)
```

## Detection strategy

Try to parse the first bytes as HTTP. If parsing succeeds within a
reasonable buffer (e.g., 64KB of header data), treat as HTTP. If it
fails or times out, fall back to raw byte relay — the already-buffered
bytes are flushed to the raw relay so non-HTTP protocols still work.

## Implementation

### Bridge: RPC bytes ↔ hyper I/O

The container's bytes arrive via `ChannelSink.send()` into an mpsc
channel. We need to present this as `AsyncRead` for hyper's server:

- Create a `ChannelReader` that implements `AsyncRead` by pulling
  from the mpsc `Receiver<Vec<u8>>` (already buffered at capacity 1)
- Similarly, create a `ChannelWriter` that sends bytes back to the
  container via the `client_sink: tcp_sink::Client` RPC

Combine into a `tokio::io::DuplexStream`-like transport that hyper
can use for `http1::Builder::new().serve_connection(transport, svc)`.

### HTTP proxy flow (per connection)

1. Container sends HTTP bytes through RPC
2. hyper server parses: `Request<Incoming>` with method, uri, headers,
   streaming body
3. Build `HttpRequest` userdata from the parsed request
4. Run all http_request Lua rules
5. If denied → send `403 Forbidden` response back via hyper
6. If allowed → forward with hyper client:
   - Build `hyper::Request` with (modified) method/path/headers/body
   - Connect to real server (reuse existing TLS connector)
   - Stream response back through hyper → RPC → container

### Lua API for http_request

New userdata type: `cli/src/network/scripting/http_request.rs`

```lua
req.host         -- string, read/write (from tcp_connect)
req.port         -- number, read/write
req.tls          -- boolean, read-only
req.method       -- string, read-only (GET, POST, etc.)
req.path         -- string, read/write
req.headers      -- table, read/write
req:allow()
req:deny()
req:hostMatches(pattern)
```

Headers table: Lua table where keys are header names (lowercase),
values are strings. Modified headers are reconstructed into the
forwarded request.

### Script engine changes

Add `intercept_http_request()` to `ScriptEngine`:

- Filters rules to `NetworkRuleType::HttpRequest`
- Creates `HttpRequest` userdata from parsed hyper request
- Same allow/deny/default flow as tcp_connect
- Returns modified request info for forwarding

### spawn_relay changes

`spawn_relay` becomes two variants:

- `spawn_raw_relay` — current behavior, raw byte passthrough
- `spawn_http_proxy` — hyper-based HTTP proxy with script interception

In `connect()`, after tcp_connect scripts pass:

1. Set up the RPC channels
2. Peek at first bytes from container
3. If HTTP-like → `spawn_http_proxy`
4. Else → `spawn_raw_relay` (flush peeked bytes first)

## Files to create/modify

- `cli/Cargo.toml` — add `hyper`, `http-body-util`, `hyper-util`
- `cli/src/network/scripting/http_request.rs` — HttpRequest userdata
- `cli/src/network/scripting.rs` — add `intercept_http_request()`
- `cli/src/network/server.rs` — split relay, add HTTP proxy path
- `cli/src/network/bridge.rs` — ChannelReader/Writer for RPC↔hyper

## Verification

```bash
# Test HTTP header injection
cat > ez.local.toml << 'EOF'
[network]
default_mode = "allow"
[[network.rules]]
name = "inject auth"
type = "http_request"
env.TOKEN = "test-token"
script = """
req.headers["X-Custom"] = "injected-" .. env.TOKEN
req:allow()
"""
EOF
# Verify header arrives at server (use httpbin or similar)
mise run ez -- curl -v http://httpbin.org/headers

# Test path rewriting
# Test deny at HTTP level (returns 403 to container)
# Test non-HTTP fallback (raw TCP still works)
```
