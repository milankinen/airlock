# vsock + Cap'n Proto RPC between CLI and supervisor

### What

Established host↔guest communication over vsock using Cap'n Proto RPC.
The supervisor runs inside the VM, listens on vsock port 1024, and
responds to RPC calls from the CLI.

### Architecture

```
Host (macOS)                          Guest (Linux VM)
CLI ──vsock──→ supervisor
     capnp-rpc (twoparty)            listens, accepts, serves RPC
```

- `VZVirtioSocketDeviceConfiguration` added to VM config
- CLI connects with `connectToPort_completionHandler`, retries until
  supervisor is ready
- Supervisor uses raw `AF_VSOCK` sockets (libc), wrapped in tokio
  `TcpStream` for the capnp-rpc twoparty transport

### Protocol crate (`protocol/`)

- Schema: `protocol/schema/supervisor.capnp` with `interface Supervisor`
- Code generated at build time by `capnpc` via `build.rs`
- Both host and Docker builds need `capnp` binary (brew on host,
  apt-get in Docker)

### Supervisor (`sandbox/supervisor/`)

Moved under `sandbox/` since it's a guest-side component built for
Linux musl via Docker. Has its own Dockerfile for the builder image
(`ezpez-supervisor-builder`) which caches rust toolchain + capnp +
musl-tools. Build is ~6s after image is cached.

### Key decisions

- **Cap'n Proto RPC** over manual serialization — gives proper
  request/response handling, streaming, pipelining. Just add methods
  to the schema interface.
- **tokio in supervisor** with minimal features (`rt`, `net`,
  `io-util`, `macros`) — needed for capnp-rpc's async transport.
  Binary is 1.8MB static musl.
- **Guest listens, host connects** — avoids ObjC delegate protocols.
  Host retries `connectToPort` every 100ms as readiness probe.
- **`LocalSet`** for CLI's tokio runtime — capnp-rpc types are `!Send`,
  require `spawn_local`.
- **Dedicated Docker builder image** — caches apt packages, rustup
  target, avoids reinstalling on every supervisor build.
