# vsock + supervisor communication

## Context

The CLI can boot a VM and give a shell, but there's no programmatic
communication between host and guest. vsock is the foundation for
everything in the design doc: container lifecycle, networking proxy,
file mounts, env injection. This step establishes the communication
channel and proves it works with a ping/pong.

## Architecture

```
Host (macOS)                          Guest (Linux VM)
┌─────────────┐                       ┌──────────────────┐
│ CLI (ez)    │──vsock port 1024────→ │ supervisor       │
│             │  length-prefixed JSON │ (listens, accepts)│
│             │←─────────────────────│                  │
│             │                       │                  │
│             │──virtio console─────→ │ /bin/sh (debug)  │
└─────────────┘                       └──────────────────┘
```

## Plan

### Phase 1: vsock device in VM config

Add `VZVirtioSocketDeviceConfiguration` to the VM config (same
pattern as entropy/balloon). New objc2-virtualization features:
`VZVirtioSocketDeviceConfiguration`, `VZSocketDeviceConfiguration`,
`VZVirtioSocketDevice`, `VZSocketDevice`, `VZVirtioSocketConnection`.

**Files:** `cli/Cargo.toml`, `cli/src/vm/apple.rs`
**Verify:** VM boots, `dmesg | grep vsock` shows device in guest.

### Phase 2: protocol crate (Cap'n Proto)

New workspace member `protocol/` with:
- `capnp` runtime dependency
- `capnpc` build dependency (code generator)
- Schema file `protocol/schema/supervisor.capnp` defining messages
- `build.rs` that runs `capnpc` to generate Rust code
- Generated code checked into `protocol/src/generated/`
- `pub const SUPERVISOR_PORT: u32 = 1024;`

Schema (MVP):
```capnp
@0xabcdef...; # unique file ID

struct PingRequest { }
struct PongResponse { }

interface Supervisor {
  ping @0 () -> (pong :PongResponse);
}
```

For the MVP we won't use Cap'n Proto RPC — just serialize/deserialize
messages over the vsock fd with Cap'n Proto's standard framing
(`capnp::serialize` read/write_message). This keeps it simple while
getting the schema benefits. RPC can be layered on later.

Both `cli` and `supervisor` depend on this crate. Code is generated
on the host (requires `brew install capnp`), checked into the repo
so the Docker supervisor build doesn't need capnpc.

**Files:** `Cargo.toml`, `protocol/Cargo.toml`, `protocol/build.rs`,
`protocol/schema/supervisor.capnp`, `protocol/src/lib.rs`,
`cli/Cargo.toml`, `supervisor/Cargo.toml`

### Phase 3: supervisor vsock listener

Implement the supervisor's vsock listener using raw `libc`:
- Define `AF_VSOCK` (40), `VMADDR_CID_ANY`, `sockaddr_vm` manually
  (libc crate doesn't include them)
- `socket(AF_VSOCK, SOCK_STREAM, 0)` → `bind` → `listen` → `accept`
- Read messages with sync protocol framing
- Respond `Pong` to `Ping`, exit on EOF
- Fully synchronous — no tokio (keeps binary <1MB musl)

**Files:** `supervisor/src/main.rs`

### Phase 4: rootfs integration

- Update `sandbox/rootfs/build.sh` to copy supervisor binary into
  rootfs before cpio packing
- Update `sandbox/rootfs/init` to launch `/usr/bin/supervisor &`
  before the shell
- `build:rootfs` depends on `build:supervisor` in mise.toml
- Build pipeline: kernel + supervisor (parallel) → rootfs → cli

**Files:** `sandbox/rootfs/build.sh`, `sandbox/rootfs/init`, `mise.toml`

### Phase 5: host vsock connect + ping/pong

Add `vsock_connect(port) -> OwnedFd` to `VmBackend` trait.
Implementation in `AppleVmBackend`:
1. Access `vm.socketDevices()` on dispatch queue
2. Downcast to `VZVirtioSocketDevice`
3. `connectToPort_completionHandler` → oneshot channel bridge
4. `dup()` the fd from `VZVirtioSocketConnection`, return `OwnedFd`

In `main.rs` after VM start:
- Retry connect every 100ms, up to 30 attempts
- Send `Ping`, await `Pong`
- Print "supervisor connected" to stderr

**Files:** `cli/src/vm/mod.rs`, `cli/src/vm/apple.rs`, `cli/src/main.rs`

### Phase 6: verify end-to-end

1. `mise run build` builds everything
2. `mise run ez -v` boots VM, connects vsock, ping/pong succeeds
3. stderr shows: "Booting VM... VM started... supervisor connected"
4. `exit` in shell → clean shutdown
5. No orphan processes

## Key decisions

- **Guest listens, host connects** — avoids implementing ObjC
  delegate protocols from Rust (VZVirtioSocketListener needs a
  delegate). Host retries connect as readiness probe.
- **Cap'n Proto** — schema-driven protocol from the start. Use
  `capnp::serialize` for framing (not full RPC yet). Schema
  naturally expresses the guest/host communication. Code generated
  on macOS host, checked into repo.
- **Sync supervisor** — no tokio in guest binary. Keeps it <1MB.
  Single connection, blocking I/O.
- **`dup()` the vsock fd** — decouple from `VZVirtioSocketConnection`
  ObjC object lifetime.
