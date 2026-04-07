# HTTP/2 support + RefCell soundness fix

### HTTP/2 support

Added end-to-end HTTP/2 support for the proxy:

- Supervisor MITM TLS config advertises h2+http/1.1 via ALPN, so
  containers can negotiate HTTP/2 during TLS handshake
- CLI TLS client config also advertises h2+http/1.1 ALPN to real
  servers
- CLI checks ALPN result from real server connection to determine
  protocol: h2 → `hyper::client::conn::http2`, h1 → http1 client
- `RequestSender` trait abstracts over h1/h2 clients so the request
  handler works with either
- `LocalExec` executor spawns h2 stream tasks via `spawn_local`
- Outgoing request uses absolute URI (`https://host/path`) so hyper
  correctly derives h2 `:authority`/`:scheme` pseudo-headers

### HTTP detection improvement

Replaced 4-byte prefix check with proper request line parsing:
buffer up to 4KB or first `\r\n`, then validate with regex
`^[A-Z]+ \S+ HTTP/\S+$` (h1) or `^PRI \* HTTP/2\.0$` (h2 preface).

### RefCell soundness fix

Audited all RefCell usage in the network module:

- **ChannelSink::tx** — was holding `Ref` across `.await` in `send()`,
  which could panic if `close()` was dispatched concurrently by
  capnp-rpc. Fixed by cloning the sender before awaiting.
- **RequestSender** — `borrow_mut()` drops before `.await` (sound)
- **RelayError** — writer sets after loop ends (sound)
