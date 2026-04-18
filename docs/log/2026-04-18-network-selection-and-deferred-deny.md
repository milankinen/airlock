# Network panel: row selection, details sub-tab, and deferred denies

## What changed

The F2 Monitor's network panel picked up a handful of tightly-related
features:

- **Default sub-tab is Requests.** Most of the time the interesting
  signal is the HTTP stream, not the TCP connect log.
- **Empty-state copy** mirrors Requests on the Connections tab ("No TCP
  connections observed yet.") instead of a blank area.
- **Lists render newest-first** so fresh events appear at the top
  without requiring a scroll-to-end.
- **Arrow keys select a row.** `↑ / ↓`, `PgUp / PgDn`, and `Home / End`
  move a highlighted row. The newest entry is selected by default and
  the selection *follows* new events (so the thing under your cursor
  stays under your cursor as new rows land).
- **`Enter` opens a third sub-tab — "Request details" / "Connection
  details" — with a clickable `×` close glyph.** Shows status, local
  timestamp, method, target, path, and captured request headers.
  Closes with `Esc`, `x`, or the `×`.
- **Request headers are now captured** at the middleware and carried
  through the event payload so the details view can render them.
- **Buffer cap of 100 entries per list**, surfaced through a new
  `TuiSettings { max_http_requests, max_tcp_connections }` rather than
  hard-coded inline. `TuiSettings` is a scaffold for future
  user-configurable TUI preferences; for now the defaults are fixed.
- **Denied HTTP requests are surfaced in the Requests sub-tab** with
  full method/path/header detail, instead of vanishing behind an early
  TCP deny.

## Event shape: Arc-wrapped payloads, receiver-gated emission

Before: `NetworkEvent` was an enum of struct variants that carried
owned `String` fields. Every emission cloned those fields unconditionally,
even when nothing was subscribed (the non-`--monitor` case).

After:

```rust
pub enum NetworkEvent {
    Connect(Arc<ConnectInfo>),
    Request(Arc<RequestInfo>),
}
```

- `emit_event` / `emit_request_event` check
  `broadcast::Sender::receiver_count()` first and return before any
  allocation if nobody is listening. This is the hot path on
  non-monitor runs — no work, no strings, no events dropped into a
  silent channel.
- Payloads are `Arc`'d once at emission; `broadcast::Receiver::recv`
  then just bumps a refcount. Important because capturing HTTP request
  headers pushes the payload size up noticeably (each header is two
  `String`s).

## Deferring denies to the relay phase

The big shift on the proxy side: a TCP `connect` RPC from the guest no
longer returns `Denied` when the policy says no. It always succeeds,
and the deny decision is applied further down the pipeline. This is
the only way we can surface denied HTTP request details at all — we
need to peek past TLS and read the request headers before refusing.

### Flow

```
guest ─── connect(host:port) ──► host resolves target
                                 (events a Connect event with allowed=T/F)
                                 always returns a sink
guest ─── bytes ──► TLS detect ──► (MITM accept if TLS)
                                ──► HTTP detect
                                ──► branch on target.allowed
                                    allowed + HTTP:  http::relay → upstream
                                    allowed + !HTTP: tcp::relay  → upstream
                                    !allowed + HTTP: http::relay → 403
                                    !allowed + !HTTP: drop
```

Concretely:

- `tls::establish` was split into `accept_container` (MITM handshake
  only, returns the negotiated ALPN) and `connect_server` (dials the
  real upstream, matching ALPN). The deny path calls only the first.
- `tcp::establish` was split into `container_transport` (wraps the
  guest's RPC channel) and `connect_server`.
- `http::relay` gained an early branch: when `target.allowed == false`,
  it serves a hyper server over the container transport that emits the
  `Request` event with `allowed=false` and responds `403 Forbidden` —
  without ever setting up a client-side hyper connection or touching
  the server transport.
- `handle_connection` no longer has allow/deny branching for the
  actual relay call. It picks one `server` transport via a single
  match, then routes to `http::relay` or `tcp::relay`:

  ```rust
  let server = match (target.allowed, is_tls) {
      (false, _)    => io::Transport::null(),
      (true, true)  => tls::connect_server(&target, alpn.as_deref(), tls_client).await?,
      (true, false) => tcp::connect_server(&addr).await?,
  };
  if is_http { http::relay(container, server, target, events).await? }
  else       { tcp::relay(container, server).await }
  ```

- `io::Transport::null()` is the unified deny-side transport: reads
  return EOF immediately (`tokio::io::empty()`), writes are discarded
  (`tokio::io::sink()`). For `tcp::relay` this collapses the loop
  immediately and closes the container; for `http::relay` we never
  reach the server side because the `!target.allowed` branch returns
  first.

### Trade-offs considered

- **Letting hyper try to talk to the null transport** (instead of
  short-circuiting in `http::relay`) would simplify the function but
  degrade the UX: h1 would succeed the handshake, fail to send, and
  surface as `502 Bad Gateway`, and h2 would fail during its preface
  exchange before the service even runs — losing the Request event
  for denied h2. Short-circuiting on `!allowed` preserves clean `403`
  semantics for both protocols.
- **Running cert generation for denied TLS hosts** is new work in the
  deny path. The per-hostname cache (bounded at 256 entries) means a
  flood of unique denied hosts is bounded; typical workloads hit a
  small set of denied endpoints, so the cost is negligible.
- **Guest-visible behavior shift**: previously denied TCP got an
  immediate RPC `Denied` (guest's proxy RSTs the socket). Now the
  guest sees a successful TCP/TLS handshake followed by either `403`
  (HTTP) or a clean close (non-HTTP). Arguably clearer — denied curl
  invocations now print `HTTP/1.1 403 Forbidden` instead of
  `Connection refused` — and matches how real firewalls often
  present policy blocks.

## Tests

Two tests assumed `connect_result::Denied` at the RPC layer:

- `host_not_allowed_is_denied`
- `empty_allowed_hosts_denies_all`

Both now assert that the connect succeeds and that a `403` response
comes back when the guest sends an HTTP request. All other network
tests pass unchanged.
