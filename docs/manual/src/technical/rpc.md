# RPC protocol

CLI and supervisor communicate over a single vsock connection (host
TCP on macOS, vsock on Linux) using [Cap'n Proto](https://capnproto.org/)
RPC in twoparty transport mode.

**Supervisor listens, CLI connects.** The CLI polls the vsock port
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
  this hard — `Supervisor.start(...)` takes a `Stdin` capability and
  a `NetworkProxy` capability as arguments, and the supervisor
  calls methods on them instead of opening its own egress. There's
  no URL, no port, no service discovery: the host chose to hand over
  a specific capability and that's the only thing the VM can invoke.
  This is how "the VM has no way out except what the host explicitly
  grants" is enforced at the protocol level, not by convention.
- **Built-in pipelining and interleaving.** Many concurrent calls
  and streaming results share the same connection without any
  multiplexing glue of our own. stdio polling, the `Stdin` read
  loop, stats polling, deny notifications, and every outbound TCP
  proxy session run simultaneously over a single socket. Writing
  this on top of a request/response RPC would mean reinventing
  multiplexing.

The combination is why a "single vsock, single session" design is
realistic in the first place — and why we don't need a virtio
console, a second vsock port, or any other sidecar transport.

## Interfaces (supervisor.capnp)

### Supervisor

Boot-time call that carries both the process configuration (replacing
a written `config.json`) and the mount configuration (replacing a
written `mounts.json`). A separate `exec` call attaches extra
processes to the running container.

```
Supervisor
  start(stdin, pty, network, logs, logFilter,
        epoch, epochNanos, hostPorts, sockets,
        cmd, args, env, cwd, uid, gid, nestedVirt, harden,
        imageId, imageLayers, dirs, files, caches, caCert) → Process
    # Triggers VM init, then forks and execs the container process
    # directly (no crun). `caCert` is the project CA PEM — appended
    # by guest init to the image's CA bundles; empty disables TLS
    # injection.

  exec(stdin, pty, cmd, args, cwd, env) → Process
    # Attach a new process to the running container. Called once per
    # `airlock exec` invocation via the CLI server bridge (below).

  shutdown() → ()
    # Sync filesystems before VM teardown.

  pollStats() → StatsSnapshot
    # CPU/memory/load-average for the Monitor TUI.

  reportDeny(epoch) → ()
    # Host → guest: a network request was denied. The guest caches
    # the timestamp so the admin HTTP service at http://admin.airlock/
    # can correlate it with Claude Code tool failures reported via
    # hook endpoints.
```

### CliService

Exposed by the running `airlock start` process over
`<project>/.airlock/sandbox/cli.sock`. `airlock exec` connects here,
the CLI server merges the sandbox's resolved base env with any
`-e KEY=VAL` overrides, and forwards the call to the in-VM
supervisor over the existing vsock.

```
CliService
  exec(stdin, pty, cmd, args, cwd, env) → Process
```

### Process / Stdin

```
Process
  poll() → (exit:Int32 | stdout:Data | stderr:Data)
  signal(signum) → ()
  kill() → ()

Stdin
  read() → (stdin:DataFrame | resize:TermSize)
```

### NetworkProxy / LogSink

```
NetworkProxy
  connect(target, clientSink) → (serverSink | denied)
    # TCP relay: guest connects to target, host bridges to the real
    # destination. `target` is either a TCP `host:port` or a Unix
    # socket guest path.

LogSink
  log(level, message) → stream
    # Guest-side tracing records streamed to the host's run.log.
```

