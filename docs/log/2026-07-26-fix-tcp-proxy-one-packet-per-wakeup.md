# Fix tunnel throughput: one packet per poll-loop wakeup

## Problem

Downloads inside the sandbox capped at ~2.5 MB/s per connection while
the host managed 10 MB/s on the same link. Curiously, a localhost
upstream benched from the host was much faster through the proxy than a
remote one — throughput appeared coupled to upstream latency.

## Diagnosis

Measured from inside the guest:

- Every upstream (hetzner, Cloudflare, npm) capped at ~2.7 MB/s per
  connection.
- Two parallel downloads reached ~7.5 MB/s aggregate — so the ceiling
  was per-connection serialization, not CPU, vsock bandwidth or the
  host-side proxy (all connections share one CLI thread, one guest poll
  loop and one vsock; a shared-resource cap wouldn't scale).

To split the tunnel in half, `tcp_proxy_bench` (new) runs the real
smoltcp poll loop on a private TUN against an in-process mock
`NetworkProxy` that blasts bytes the way the CLI does — no vsock, no
host, no TLS. It reproduced the ceiling exactly: **2.4 MiB/s**. The
guest-side stack was the bottleneck.

Loop counters (also new, `cfg(test)` only) made the mechanism obvious:

```
tun_bench_download: 64 MiB in 27.08s = 2.36 MiB/s
  loop: 46084 iters (1831/s), wake fd=23120 notify=4097 timer=18866
  pkts: tx=46082 (1831/s) ...
```

`tx == iters` — exactly **one 1456-byte segment per loop wakeup**.
Socket buffer size and channel capacity experiments (16 KB → 256 KB,
8 → 64) changed nothing, because the cap was
`wakeup_rate × one MSS ≈ 1831/s × 1456 B ≈ 2.5 MiB/s`.

## Root cause

smoltcp's `poll_egress` is documented as "guaranteed to always perform
a bounded amount of work": one call emits **at most one packet per
socket** (a single `socket.dispatch`). smoltcp's own `poll()` drives it
in a loop until it reports `PollResult::None`. Our poll loop called it
**once** per iteration, so every wakeup (ACK arrival, notify, or
smoltcp timer) released exactly one segment.

This also explains the latency coupling the user observed: with a
one-packet-per-wakeup budget, throughput is set entirely by the wakeup
cadence — which chunk-arrival timing (and therefore upstream latency)
modulates — instead of by available window.

## Fix

Drive `poll_egress` until it reports no progress, mirroring upstream
`poll()`:

```rust
while iface.poll_egress(now, &mut device, &mut sockets) == PollResult::SocketStateChanged
{}
```

The work stays bounded — a socket can't emit more than its (16 KB) tx
buffer per pass, and the loop ends when every socket is drained.

Isolated benchmark, before → after:

- download: **2.4 → ~490 MiB/s** (timer wakes drop from 18866 to 0 —
  the loop becomes purely event-driven)
- upload: ~190 MiB/s (was not measured before the fix; the guest→host
  path awaits each `TcpSink.send` and was capped by the same
  one-ACK-per-wakeup effect)

Buffer sizes (`RX_BUF`/`TX_BUF` 16 KB, `CHAN_CAP` 8) were left
unchanged — with egress draining properly they are not the limiting
factor at any realistic upstream speed, and small buffers keep
backpressure tight.

End-to-end throughput needs verification after rebuilding airlockd +
initramfs and restarting the sandbox (the benchmark isolates the guest
half; vsock and the host CLI add their own, previously-masked, costs).

## Benchmark infra added

- `tcp_proxy::spawn_poll_loop` split out of `start()` so the benchmark
  can run the identical loop on a private TUN (`bench1`/`bench2`)
  whose bring-up uses SIOC* ioctls — the guest image has no `/sbin/ip`.
- `net/tcp_proxy_bench.rs`: `tun_bench_download` / `tun_bench_upload`.
  Creating TUN devices needs root + CAP_NET_ADMIN, so the benchmark is
  double-gated: compiled only with the `tun-bench` cargo feature and
  `#[ignore]`d at runtime:

  ```sh
  cargo test -p airlockd --release --features tun-bench tun_bench -- \
      --ignored --nocapture --test-threads=1
  ```

- `tcp_proxy::stats` counters (feature-gated the same way): loop
  iterations, wake sources (fd/notify/timer), tx/rx packets, bytes
  entering sockets.
