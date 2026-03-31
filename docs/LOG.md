# Development Log

## 2026-04-01: Pull-based exec protocol with PTY size and resize

### What

Redesigned the RPC protocol to pull-based: CLI calls `proc.poll()`
for output, supervisor calls `stdin.read()` for input. Added optional
PTY config (size or none) to `exec`, `resize` to `Process`, and
SIGWINCH handling to propagate terminal resizes.

### Schema

```
exec(stdin :ByteStream, pty :PtyConfig) -> (proc :Process)
Process { poll, signal, kill, resize }
ByteStream { read -> DataFrame(eof|data|err) }
ProcessOutput(exit|stdout|stderr)
PtyConfig(none|size(rows,cols))
```

### Key decisions

- **Pull-based read()** over push-based write() — cleaner API, inline
  error handling, natural backpressure. Vsock latency is negligible.
- **Optional PTY** — `pty: none` for future non-interactive exec
  (separate stdout/stderr). Shell always gets a PTY.
- **SIGWINCH** → `proc.resize()` — spawned as `spawn_local` task
  alongside the poll loop. No-op if process has no PTY.
- **Removed old console relay** — all I/O goes through RPC now.

## 2026-04-01: Route shell through supervisor PTY over RPC

### What

Shell I/O now flows through the supervisor via Cap'n Proto RPC instead
of the virtio console relay. The supervisor spawns a shell on a proper
PTY (`/dev/pts/0`), eliminating the "can't access tty; job control
turned off" warning.

### Architecture

```
CLI stdin → RPC stdin.write() → supervisor → PTY master → shell
CLI stdout ← RPC stdout.write() ← supervisor ← PTY master ← shell
                                  stdout.done(exitCode) on shell exit
```

### Schema

```capnp
interface Supervisor {
  openShell (rows, cols, stdout :OutputStream) -> (stdin :OutputStream);
}
interface OutputStream {
  write (data :Data) -> stream;
  done (exitCode :Int32) -> ();
}
```

Both directions use push-based `OutputStream` callbacks over the same
vsock connection. Cap'n Proto RPC multiplexes everything automatically.

### Key decisions

- **Push-based both directions** — CLI passes `stdout` callback (receives
  output), gets back `stdin` capability (sends input). No polling, no
  second vsock port. capnp-rpc handles multiplexing.
- **`pty-process` crate** — replaces ~100 lines of raw libc (`openpty`,
  `fork`, `setsid`, `ioctl`, `dup2`, `execl`, `fcntl`, `AsyncFd`)
  with ~30 lines. Integrates with tokio `AsyncRead`/`AsyncWrite`.
- **Exit code propagation** — supervisor awaits `child.wait()`, sends
  exit code via `stdout.done(exitCode)`, CLI receives via oneshot
  channel and calls `std::process::exit(code)`.
- **Bookworm for Docker builder** — bullseye's capnp 0.7.0 doesn't
  support `-> stream` syntax. Bookworm has 0.9.2.

## 2026-03-31: vsock + Cap'n Proto RPC between CLI and supervisor

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

## 2026-03-31: Single-binary with embedded kernel and rootfs

### What

Kernel and rootfs are now built locally via `docker run` and embedded
into the `ez` binary with `include_bytes!`. No runtime downloads —
ship one binary that boots a full Linux VM.

### Build pipeline

Kernel (6.18.3) is built from source using PUI PUI Linux's defconfig
(`sandbox/kernel/config-arm64`). Rootfs is an Alpine 3.23 minirootfs
with a custom `/init` script, packed as a cpio initramfs. Both are
built inside ephemeral Docker containers and output to `sandbox/out/`.

`mise run build` orchestrates: kernel → rootfs → cargo build + codesign.
All tasks have `sources`/`outputs` for incremental rebuilds (2ms no-op).

### Cargo workspace

Project restructured into a workspace:
- `cli/` — the `ez` binary (macOS host, Apple Virtualization)
- `supervisor/` — agent that will run inside the VM (built for Linux
  via Docker with `rust:alpine`)

Workspace root manages shared dependency versions.

### Key decisions

#### Embedded assets via include_bytes!

VZLinuxBootLoader requires file URLs, so embedded bytes are written
to a `tempfile::TempDir` at startup. The temp dir is kept alive for
the process lifetime via the `AssetPaths._tmp` field. This adds ~12MB
to the binary (7MB kernel + 5MB rootfs) but eliminates all runtime
download code (reqwest, sha2, tar, flate2, cpio all removed).

#### docker run over docker build

Build scripts (`sandbox/*/build.sh`) are mounted into ephemeral
`alpine:3.23` containers. No Dockerfiles, no image layers to manage.
Cargo cache for supervisor builds uses a named Docker volume
(`ezpez-cargo-cache`) to avoid re-downloading crates.

## 2026-03-30: MVP — Boot a Linux VM with interactive shell

### What

Implemented the first working version of ezpez: a CLI that boots a
lightweight Linux VM on macOS and gives the user an interactive shell.

### Architecture

```
CLI (clap) → Asset Manager → Apple Virtualization backend → Terminal relay
```

- **CLI** (`src/cli.rs`): `ez [--cpus N] [--memory N] [--kernel PATH]
  [--initramfs PATH] [--verbose]`
- **Asset Manager** (`src/assets/`): Downloads and caches a Linux
  kernel + Alpine minirootfs, builds a custom initramfs with an
  `/init` script
- **VM Backend** (`src/vm/apple.rs`): Configures and boots a VM using
  Apple's Virtualization framework via `objc2-virtualization` bindings.
  Uses a serial dispatch queue for VM operations, communicates with
  tokio via oneshot channels.
- **Terminal Relay** (`src/terminal/mod.rs`): Bidirectional stdin/stdout
  relay between the user's terminal and the VM's virtio console. Raw
  mode when running on a TTY.
- **VmBackend trait** (`src/vm/mod.rs`): Abstraction for future Linux
  (`libkrun`) support.

### Key decisions and lessons

#### Kernel format: VZLinuxBootLoader rejects EFI stub kernels

Alpine's `vmlinuz-virt` is compiled with `CONFIG_EFI_STUB=y`, which
places a PE/COFF `MZ` header at offset 0 of the Image. Apple's
`VZLinuxBootLoader` does direct boot (jumps to offset 0) and silently
fails with `VZErrorInternal` (code 1) when it encounters a PE header.

The error message is completely unhelpful — "Internal Virtualization
error. The virtual machine failed to start." — with no indication that
the kernel format is the issue. Discovered this by:
1. Testing with Docker's LinuxKit kernel (works — starts with ARM64
   NOP, no PE header)
2. Comparing binary headers between working and failing kernels
3. Web research confirming VZLinuxBootLoader requires non-EFI-stub
   kernels

**Resolution**: Use [PUI PUI Linux](https://github.com/Code-Hex/puipui-linux)
kernel (5MB, built specifically for Apple Virtualization, no EFI stub).

**Alternatives considered**:
- Kata Containers kernel (`vmlinux` ELF format) — works but 400MB+ download
- Building custom kernel — too complex for MVP
- `VZEFIBootLoader` — would handle EFI kernels but requires disk images,
  EFI variable store, and more complex boot flow
- Stripping PE header from Alpine kernel — doesn't work, the binary
  structure differs fundamentally beyond just the header bytes

#### macOS entitlements

Both `com.apple.security.virtualization` AND
`com.apple.security.hypervisor` entitlements are required. The binary
must be ad-hoc codesigned after every `cargo build`. This is automated
via `mise run build`.

#### Threading: objc2 + dispatch queue + tokio

`VZVirtualMachine` is `!Send` (ObjC objects contain raw pointers) and
must be used from its serial dispatch queue. Solution:
- Store the VM pointer as `usize` to cross thread boundaries
- `unsafe impl Send` on the backend struct
- All VM method calls dispatched to the queue via `exec_async`
- Completion handlers bridge to tokio via `Mutex<Option<oneshot::Sender>>`
  (needed because `RcBlock` requires `Fn`, not `FnOnce`)
- Use `tokio::main(flavor = "current_thread")` since Send isn't needed

#### Init script: PID 1 signal handling

`exec /bin/sh` as init makes the shell PID 1. Ctrl+C sends SIGINT
which kills init → kernel panic. Fix: trap signals in init, run shell
as child process via `setsid`, then `poweroff -f` on shell exit.
