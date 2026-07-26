# Fix HTTP/2 requests to HTTP/1.1-only upstreams

## Problem

Inside the sandbox, HTTPS requests to some hosts died with an opaque
TLS error while the very same URL worked over HTTP/1.1:

```
$ curl -v 'https://ash-speed.hetzner.com/100MB.bin' -o /dev/null
* SSL connection using TLSv1.3 / TLS_AES_256_GCM_SHA384
* ALPN: server accepted h2
* Server certificate: subject: CN=airlock ash-speed.hetzner.com
> GET /100MB.bin HTTP/2
* Request completely sent off
* TLSv1.3 (IN), TLS handshake, Newsession Ticket (4):
* TLSv1.3 (OUT), TLS alert, decode error (562):
* OpenSSL SSL_read: error:0A000126:SSL routines::unexpected eof while reading
curl: (56) ...unexpected eof while reading
```

`curl --http1.1` on the same URL downloaded fine, and HTTP/2 to other
hosts (Cloudflare) worked. It was not size-related — a `--range 0-1000`
and even a `HEAD` failed the same way.

The verbose trace pins it down: `ossl_bio_cf_in_read(len=5) -> 0` is a
clean TCP FIN with **zero** application bytes, 217 ms after the request.
Nothing is corrupted; the proxy tears the connection down before writing
anything, and 217 ms is about the time to reach the upstream.

## Root cause 1: ALPN offered to the upstream is too narrow

`TlsInterceptor::get_or_create_config` always advertises
`["h2", "http/1.1"]` to the container — it has to, since the container's
handshake happens long before we know anything about the real server.
When the container picked `h2`, `tls::connect_server` then offered the
upstream **only** `h2`:

```rust
config.alpn_protocols = match alpn {
    Some(proto) => vec![proto.to_vec()],
    None => vec![b"http/1.1".to_vec()],
};
```

An upstream that only speaks HTTP/1.1 — hetzner's speed-test endpoint
among them — aborts that handshake with `no_application_protocol`.
`tls::connect_server` returns `Err`, `handle_connection` unwinds, and the
guest-side transport is dropped. The guest gets a bare FIN in the middle
of an established TLS session, which surfaces as "unexpected eof" with
the actual reason only in the host's debug log.

Protocols never had to match across the proxy: `http::relay` picks its
upstream client from the **server's** negotiated protocol
(`if server.h2 { H2Sender } else { H1Sender }`), independently of what
the container negotiated. So the fix is to keep `http/1.1` as a fallback
and let the proxy bridge h2 → http/1.1.

## Root cause 2: forwarded h2 requests are not valid HTTP/1.1

With the ALPN fallback in place the connection survived but nginx
answered `400 Bad Request`. A request that arrived over h2 carries its
authority in the URI (built from `:authority`) and has no `Host` header,
because h2 has none. Handed to hyper's h1 client verbatim that goes out
as

```
GET https://ash-speed.hetzner.com/100MB.bin HTTP/1.1
(no Host header)
```

`H1Sender` now rewrites the request into the origin-form + `Host` pair
h1 origin servers expect. Requests that arrived over h1 are already in
origin form and carry their own `Host`, so it is a no-op for them.

The same rewrite covers h2-with-prior-knowledge over cleartext, where
the upstream is a plain socket and therefore never h2. That path was
observably broken before the fix — `curl --http2-prior-knowledge
http://ash-speed.hetzner.com/100MB.bin` returned nginx's `HTTP/2 400`
while `--http1.1` on the same URL returned `206` — and goes through the
same `H1Sender`, so it is fixed by the same change.

## Tests added

- `h2_container_h1_only_upstream`: container negotiates h2 with the
  MITM against an upstream that only offers `http/1.1`. Fails with the
  exact `unexpected eof / no close_notify` error before the first fix,
  then with a wrong request line before the second. Asserts the
  upstream sees `GET /echo` with a correct `Host`.
- `h2_mitm_large_body_arrives_intact`: first end-to-end h2 test through
  the MITM (the existing `alpn_container_h2_server_h2` only checked ALPN
  negotiation, noting "we can't actually send h2 frames manually").
  Drives a real hyper h2 client over the RPC stream.
- `tls_mitm_large_body_arrives_intact`: same for h1, 8 MB byte-exact.

Added a `serve_https_h2` helper (h2-only HTTPS test server) and a
`LocalExec` hyper executor, since the RPC stream is `!Send` and its
connection task has to stay on the `LocalSet`.

## Related findings, not fixed here

Two separate defects turned up while chasing this. Both are independent
of the bug above and are left alone to keep this change focused.

**`RpcTransport` throws away Cap'n Proto streaming flow control.**
`TcpSink.send` is a `-> stream` method: awaiting the promise it returns
*is* the backpressure mechanism (`FixedWindowFlowController`, 64 KiB
window). `io::RpcTransport::poll_write` does `drop(req.send())` and
reports every buffer as written, so the host can queue an entire
response into the guest's RPC connection as fast as it reads it from
upstream — the guest spawns a parked `ChannelSink::send` handler per
chunk, each holding its own copy. `airlockd::logging::forward` drops the
promise of the `-> stream` `LogSink.log` the same way.

**`http::relay`'s upstream-close mirroring can truncate a response.**
Restoring the flow control above makes `tls_mitm_large_body_arrives_intact`
fail deterministically at 8 151 018 of 8 388 608 bytes, and removing
just the `connection.as_mut().graceful_shutdown()` call from the
`upstream_conn` arm of the `select!` fixes it. Once writes to the guest
can block, hyper's h1 connection reaches an idle state with megabytes
still buffered downstream; `graceful_shutdown` on an idle h1 connection
takes `Conn::disable_keep_alive`'s `state.close()` path and the
connection is closed before that buffer drains. The race is latent
today only because unthrottled writes let hyper hand the whole body off
before the upstream connection future completes. Fixing the flow control
requires fixing this first.
