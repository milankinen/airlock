# Fix lost wake-up and missing backpressure in the network proxy

## Motivation

Parallel agent work inside the sandbox felt slow. Measurements showed that
short requests ran fully in parallel (same host and different hosts), so the
proxy does not serialize connections. A source review of the byte relay
found two flow-control defects instead, one in each direction.

## Guest → host: lost wake-up in the smoltcp poll loop

`tcp_proxy::run_fsm` moves guest bytes from the smoltcp socket into the
per-connection `to_host` channel (capacity `CHAN_CAP` = 8) only while
`try_reserve` succeeds. When the channel was full the bytes stayed in
smoltcp and the advertised window closed. `relay_agent` later drained the
channel, but nothing woke the poll loop, which slept until smoltcp's next
timer or the 100 ms safety net. Uploads bottlenecked upstream (large LLM
request bodies, several agents uploading at once) therefore stalled in
~100 ms steps.

Fix: `relay_agent` pings the shared `wake` Notify right after each `recv`,
before awaiting the RPC send, so `run_fsm` refills the freed slot while the
send is in flight. `Notify::notify_one` stores a permit when no task waits,
so there is no race with the poll loop's `select!`. Repeated pings coalesce
into one extra iteration. Waking only when the channel had been full was
considered and rejected: it adds a subtle invariant for no measured gain.

Review of the same path found related teardown gaps:

- The guest `ChannelSink` only woke the loop on `send`/`close`. The host can
  release the sink without `close` (denied or failed connect, transport
  error), which dropped `from_host_tx` silently and delayed the guest FIN by
  up to 100 ms. `ChannelSink` now wakes on `Drop`, and `relay_agent` wakes on
  every exit path.
- If the host side stopped accepting bytes (connect failed or a host-side
  `send` failed) while the guest still had unread data, `try_reserve`
  returned `Closed` and `run_fsm` just stopped draining. The window stayed at
  zero and the guest hung until the host closed. `run_fsm` now sets a
  `discard_rx` flag, drops `to_host`, and drains-and-discards the smoltcp
  receive buffer on every pass. Termination goes through the existing
  `host_closed → sock.close()` path, so a response the server already sent
  (for example an early 413 during an upload) still reaches the guest,
  followed by FIN. An `abort()`-based version was rejected for two reasons:
  it discarded that response, and smoltcp emits the RST only in
  `poll_egress`, after `run_fsm` has already reaped the socket, so no RST
  went out.

## Host → guest: fire-and-forget streaming sends

`RpcTransport::poll_write` did `drop(req.send())` and returned `Ready`.
`TcpSink.send` is a capnp `-> stream` method. In capnp-rpc 0.26 the message
is written eagerly and the returned promise only signals that the per-stream
window (64 KiB) has room again, so dropping it ignored flow control. Host
writes never blocked, upstream reads were unthrottled, data piled up without
bound in the capnp write queue and in pending guest calls, a bulk download
could hog the shared vsock, and errors from the guest sink were lost.

Fix: `RpcTransport` stores the promise in `write_ack`. `poll_write` first
polls the outstanding promise (`Pending` → `Pending`, error → `BrokenPipe`)
and only then issues the next send. The promise resolves immediately while
the window has room, so steady-state throughput is unchanged. `poll_flush`
drives the promise. `poll_shutdown` flushes, sends `close` once, and awaits
its reply. The old code dropped the `close` promise, which sends a `Finish`,
and the guest could cancel the queued `close` because of it.

Awaiting `close` introduced a new wait: on the guest, calls on one
capability queue behind an in-flight streaming `send`, so `close` waits until
the guest app reads. `tcp::relay` now runs both shutdowns concurrently under
`RELAY_SHUTDOWN_TIMEOUT` (30 s) and, while they run, reads and discards both
read halves. Without the drain, a guest that writes without reading while the
server sends a large response would wait for the full timeout. Both halves
are drained because `reverse_forward` passes the guest transport as the
`server` argument. The drained bytes are ones the old code never forwarded
either.

`tests/helpers.rs::RpcStream` has the same fire-and-forget pattern but is
test-only, and was left unchanged.

## Tests

- `network::io::tests::poll_write_blocks_until_previous_send_acks_then_wakes`:
  a gated sink and a flag-setting waker check that the second write is
  `Pending`, that releasing the gate wakes the writer, and that the retry
  succeeds. Local capabilities do not use the two-party window, but local
  streaming dispatch blocks later calls behind the current one, which gives
  the same shape.
- `relay_drains_container_so_gated_shutdown_does_not_hang` in `test_tcp.rs`:
  a container whose shutdown completes only after its reader is drained. The
  fake reader starts not-ready so that the main relay loop cannot open the
  gate first. With the drain disabled, the test failed 6 out of 6 runs.
- `tcp_proxy.rs` has no unit tests. Its changes were reviewed against tokio
  and smoltcp sources. No `tun-bench` or in-VM throughput run was done for
  this change.
