# Split NetworkProxy onto its own vsock connection

Everything host ↔ guest used to share one vsock TCP connection and
one Cap'n Proto `RpcSystem`. That means pty bytes, `pollStats`,
`pollDaemons`, and bulk network traffic (`NetworkProxy.connect`
transfer payloads) all multiplexed onto one stream. Concern: a large
inbound download could saturate capnp's write queue or the kernel's
socket buffer and stall interactive traffic — keystrokes appearing
a beat late, monitor-tab polling jittering, daemon shutdown spinners
freezing.

## Shape

Two vsock connections, two `RpcSystem`s, independent socket buffers:

| port | capnp server | capnp client | carries |
|------|---------------|---------------|---------|
| `SUPERVISOR_PORT` (1024) | guest (airlockd) | host (CLI) | supervisor RPC, pty, logs, stats, daemons |
| `NETWORK_PORT` (1025, new) | host (CLI's `Network`) | guest (airlockd) | `NetworkProxy.connect` + byte relays |

`NetworkProxy` is no longer passed as a capability through
`Supervisor.start` — it's the bootstrap capability of the second
RpcSystem. That enforces the separation cleanly: supervisor code
never sees a network sink, network code never sees a supervisor call.

The guest accepts both connections; capnp *side* is independent of
vsock direction. For the network channel the guest is `Side::Client`
and the host is `Side::Server` (host provides the `NetworkProxy`
bootstrap).

## Schema split

Protocols went from one file to three, matching the runtime split:

- `schema/supervisor.capnp` — `Supervisor` RPC + process / pty / log
  / stats / daemon / mount primitives. `start` no longer takes
  `network`. Imports `network.capnp` for `TcpSink` which is still
  needed by `openLocalTcp` (host → guest reverse forward).
- `schema/network.capnp` — `NetworkProxy`, `TcpSink`, connect-target
  types. Self-contained.
- `schema/cli.capnp` — `CliService` for `airlock exec` over the local
  unix socket. Imports `supervisor.capnp` to reuse `Stdin` / `Process`
  / `PtyConfig`. Moved out of the supervisor schema because it's
  served by `airlock start`, not by the guest.

`airlock-common` now exposes three generated modules
(`supervisor_capnp`, `network_capnp`, `cli_capnp`) and a new
`NETWORK_PORT` constant.

## Runtime flow

Guest (`airlockd/src/main.rs`):

1. `vsock::listen(SUPERVISOR_PORT)` + `accept` (existing).
2. `vsock::listen(NETWORK_PORT)` + `accept` — new second connection.
3. Build an RpcSystem on the network fd with `Side::Client`, grab
   the bootstrap as `NetworkProxy::Client`, hand it to `rpc::start`.
4. The supervisor's `SupervisorImpl` stores that client and copies it
   into `StartConfig.network` for each `start()` call, so
   downstream net modules keep seeing the same shape as before.

Host (`airlock-cli/src/cli/cmd_start.rs`):

1. `vm::start` opens the supervisor vsock (existing).
2. `vm.vsock_connect(NETWORK_PORT)` opens the second one — new
   method on `VmInstance`, implemented via a new `VmHandle::
   vsock_connect(port)` trait method on both backends.
3. `rpc::serve_network(fd, network)` consumes the `Network` value
   and spawns an `RpcSystem` serving it as the `NetworkProxy`
   bootstrap. No more `set_network(...)` on the start request.

`Supervisor::start` on the host now takes `socket_fwds:
&[(String, String)]` pre-extracted from the network rather than the
whole `Network` — removes the "consume network partially before the
RPC call" dance.

## Backends

`VmHandle` gained a second trait method `vsock_connect(port)`. Both
backends have an `async vsock_connect(port)` already (apple) or a
sync version (cloud-hypervisor) that's parameterised by port. The
trait wraps them into a boxed future.

The retry-until-guest-ready loop used to be duplicated three times
(apple boot path, cloud-hypervisor boot path, and the new secondary
vsock opener). Now it lives in one place — `VmInstance::vsock_connect`
— and every caller uses it: `vm::start` for the supervisor fd,
`cmd_start` for the network fd. `boot_backend` just starts the
backend process and returns the handle, no fd and no retry.

## Trade-offs

- **No capability passing across RpcSystems**. Each capnp RpcSystem
  has its own capability table; you can't serialize a capability
  from A into a message on B. In this split that is a *feature* —
  supervisor code can't accidentally reach into network state and
  vice versa.
- **Virtio-vsock still has one virtqueue** per direction. At extreme
  burst the queue itself could backpressure. Going past that would
  mean something like virtio-net for the network channel. Out of
  scope.
- **Two accept()s on the guest side** mean the host must connect to
  the supervisor port first, then the network port. The guest's
  main.rs enforces this ordering; the CLI respects it.
