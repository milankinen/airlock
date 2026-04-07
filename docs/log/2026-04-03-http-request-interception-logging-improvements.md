# HTTP request interception + logging improvements

### HTTP interception via hyper

Added `http_request` rule type that intercepts HTTP traffic at the
application layer. When http_request rules are configured, the relay
detects HTTP by peeking at the first bytes, then hands off to hyper.

Architecture: hyper server parses requests from the container side
(via `RpcTransport` bridging the RPC byte channel into AsyncRead/
AsyncWrite), while hyper client forwards to the real server (via
`CombinedStream` reuniting the split read/write halves). Bodies
stream through without buffering.

Per-request flow:

1. hyper server parses `Request<Incoming>` from container
2. Extract method/path/headers, run http_request Lua scripts
3. Scripts can modify headers (e.g., inject auth tokens) and path
4. If denied → return 403 to container via hyper
5. If allowed → build modified request, `sender.send_request()` to
   real server, stream `Incoming` response back

Keep-alive handled automatically by hyper's `serve_connection`.
Falls back to raw byte relay if first bytes aren't HTTP.

### Relay error propagation

Shared `RelayError` cell (`Rc<RefCell<Option<String>>>`) between the
relay task and `ChannelSink`. When the relay fails, it writes the
error; `ChannelSink::send()` checks and returns it to the supervisor
so the container's connection is closed with a meaningful error.

### Logging improvements

- CLI tracing writes to `ez.log` (no ANSI colors) instead of stderr
  to avoid interfering with shell stdout
- Verbose mode uses target filtering: `warn,ez=debug` on CLI,
  `ezpez_supervisor` prefix filtering on supervisor
- Verbose flag passed to supervisor via RPC `start(verbose :Bool)`
- Error levels: vsock/RPC failures → `error!`, remote server/VM app
  events → `debug!`, fine-grained flow → `trace!`
