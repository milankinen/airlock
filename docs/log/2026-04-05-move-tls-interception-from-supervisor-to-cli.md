# Move TLS interception from supervisor to CLI

The supervisor was doing TLS MITM (terminating container TLS, re-encrypting
to the real server). This caused an ALPN mismatch: the container negotiated
h1 with the supervisor, but the CLI independently negotiated h2 with the
real server. The raw byte relay forwarded h1 bytes to an h2 server, which
rejected them.

### Fix: CLI handles all TLS

The supervisor is now a pure TCP relay — just SO_ORIGINAL_DST + DNS reverse
lookup + raw byte forwarding. All TLS logic moved to the CLI:

1. `tls::detect` reads incrementally from the RPC channel, validates the
   full TLS record header via `tls-parser` (not just first byte 0x16),
   reads the complete ClientHello record
2. If TLS: `tls::establish` does MITM via `RpcTransport` + `TlsAcceptor`,
   extracts SNI for cert generation, connects to real server with the
   **same ALPN** the container negotiated — no more mismatch
3. If not TLS: `tcp::establish` bridges the RPC channel to a TCP connection

### Network module restructuring

Split the monolithic `server.rs` into focused modules:
- `io.rs` — `Transport` (boxed read/write + h2 flag), `PrefixedRead`,
  `RpcTransport`, `ChannelSink`
- `tcp.rs` — `establish()` (plain TCP) + `relay()` (bidirectional)
- `tls.rs` — `detect()`, `establish()`, `TlsInterceptor`, `extract_sni()`
- `http.rs` — `detect()`, `relay()` (hyper proxy with Lua interception)
- `server.rs` — orchestration: connect handler, detection, routing

### Other improvements

- `http::detect` and `http::serve` now take generic `AsyncRead`/`AsyncWrite`
  instead of `mpsc::Receiver` + `tcp_sink::Client`
- Replaced `CombinedStream` with `tokio::io::join()`
- Replaced `Vec<u8>` with `Bytes`/`BytesMut` throughout network code
- TLS detection uses `tls-parser` crate for proper record header validation
  (prevents false positives from non-TLS data starting with 0x16)
- Supervisor drops `rustls`, `rcgen`, `tokio-rustls`, `quick_cache` deps
- Removed `caCert`/`caKey` from start RPC, `tls` flag from connect RPC
