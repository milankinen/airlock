# RPC protocol

CLI and supervisor communicate over two vsock connections (host
TCP on macOS, real vsock on Linux), both carrying
[Cap'n Proto](https://capnproto.org/) RPC in twoparty transport mode:

- **Supervisor channel** (`SUPERVISOR_PORT`, 1024) — everything
  except network bytes: `boot`/`spawn`, stdio, stats, daemon control,
  logs, clipboard, browser.
- **Network channel** (`NETWORK_PORT`, 1025) — `NetworkProxy` and its
  per-connection byte sinks. The split gives bulk transfers their own
  socket buffers so they cannot stall PTY or stats traffic.

**Supervisor listens, CLI connects.** The CLI polls the vsock ports
after VM boot until the supervisor is ready. This avoids implementing
callback delegates in the host virtualization API.

## Why Cap'n Proto

Two properties of Cap'n Proto made it a much better fit for this
design than a more conventional IDL (gRPC/protobuf, JSON-RPC,
bespoke framing):

- **Zero-copy wire format.** Messages are already in their in-memory
  layout when they hit the socket — no parse step, no allocation per
  field. For the stdio hot path (every keystroke and every chunk of
  container output crosses the vsock) this matters: there's no
  decode tax on top of vsock latency.
- **Remote interfaces (capabilities).** Cap'n Proto RPC is object-
  capability style: an RPC argument can itself be an interface
  reference the other side can call back through. airlock leans on
  this hard — `Supervisor.spawn(...)` takes a `Stdin` capability as
  an argument, and the guest receives the `NetworkProxy` capability
  as the bootstrap of the network channel. The supervisor
  calls methods on them instead of opening its own egress. There's
  no URL, no port, no service discovery: the host chose to hand over
  a specific capability and that's the only thing the VM can invoke.
  This enforces "the VM has no way out except what the host explicitly
  grants" at the protocol level, not by convention.
- **Built-in pipelining and interleaving.** Many concurrent calls
  and streaming results share the same connection without any
  multiplexing glue of our own. stdio polling, the `Stdin` read
  loop, stats polling, deny notifications, and every outbound TCP
  proxy session run simultaneously over a single socket. Writing
  this on top of a request/response RPC would mean reinventing
  multiplexing.

The combination is why an "RPC over vsock, nothing else" design is
realistic in the first place — and why we don't need a virtio
console or any other sidecar transport.

## Interfaces

The authoritative interface definitions live in the repository:
`app/airlock-common/schema/supervisor.capnp` (supervisor channel) and
`app/airlock-common/schema/network.capnp` (network channel). The
manual does not repeat them — the schemas are commented and always
current. A short orientation:

- **`Supervisor`** — the supervisor channel's bootstrap capability.
  `boot` prepares the container and carries the full mount, daemon,
  mask, clipboard and browser configuration — there is no
  `config.json` or `mounts.json` on disk. `boot` starts no process.
  `spawn` starts a process: the main shell, an `airlock exec` process
  or a pack install. Other calls poll stats and daemon state, bridge
  host → guest TCP, re-sync the guest clock, report network denies,
  and drive shutdown. A second `boot` and a `spawn` before `boot` are
  refused.
- **`CliService`** — exposed by the running `airlock start` process
  over `<project>/.airlock/sandbox/cli.sock`. `airlock exec` connects
  here. The CLI server merges the sandbox's resolved base env with
  any `-e KEY=VAL` overrides and forwards the call to the in-VM
  supervisor over the existing vsock.
- **`Process` / `Stdin`** — per-process capabilities for the
  pull-based stdio protocol. The CLI polls `Process` for output and
  the exit code. The supervisor pulls keyboard data and terminal
  resizes through `Stdin`. See
  [Container execution](./container-execution.md).
- **`NetworkProxy`** — the network channel's bootstrap capability.
  The guest calls it once per outbound TCP connection or Unix socket,
  and the host bridges bytes to the real destination. See
  [Networking](./networking.md).
- **`LogSink`** — guest-side tracing records, streamed to the host's
  `.airlock/airlock.log`.
- **`Clipboard`** — host clipboard access, handed to the guest inside
  `boot`'s clipboard config. A null capability is the ungranted
  state — there is no guest-side flag to subvert.
- **`Browser`** — opens a sign-in page in the host browser, for the
  [network services](./services.md). The guest gets it inside `boot`
  only when a service is on. A null capability is the ungranted state.
