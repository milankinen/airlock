# Development Log

## 2026-04-06: Fix VM networking by disabling TSI socket hijacking

Networking (DNS, TCP proxy) was broken on Linux because libkrun's
implicit vsock device enables TSI (Transparent Socket Impersonation)
with `TSI_HIJACK_INET`, which intercepts ALL AF_INET socket calls and
routes them through the host via vsock. This caused the supervisor's
DNS server (`10.0.0.1:53`) and TCP proxy (`127.0.0.1:15001`) to never
receive local connections — epoll notifications never fired because
the sockets were silently hijacked.

Fix: call `krun_disable_implicit_vsock()` then `krun_add_vsock(ctx, 0)`
to create an explicit vsock device with TSI disabled (flags=0). The
supervisor's port mapping via `krun_add_vsock_port2` still works for
the RPC channel. DNS resolution and TCP proxy redirect now functional.

## 2026-04-06: Build libkrunfw from source with netfilter support

The stock libkrunfw kernel has `CONFIG_NETFILTER` disabled, so iptables
rules for the network proxy (transparent TCP redirect to port 15001)
silently failed. Built libkrunfw from source in Docker with netfilter
config additions (nf_tables, iptables, conntrack, NAT redirect).

The build script now builds both libkrunfw (kernel) and libkrun (VMM) in
Docker containers from pinned versions (libkrunfw v5.3.0, libkrun v1.17.4).
Netfilter options are in a separate `netfilter.cfg` file appended to the
libkrunfw kernel config before compilation.

## 2026-04-06: Move VM init setup into supervisor, add block device support

The shell init script was causing race conditions and hangs under libkrun,
especially with cache disk attached. Moved all VM setup (virtiofs mounts,
networking, cache disk, DNS) from the init script into the supervisor binary.

### Why

Under libkrun, the init script runs via init.krun's chroot/exec. When the
cache disk was attached, mkfs.ext4 took time and the host vsock connection
would arrive before the supervisor started, causing RST and connection
failures. Moving the setup into the supervisor means it happens AFTER the
RPC connection is established — errors get proper logging via LogSink.

### Changes

- **RPC schema**: extended `start()` with `shares`, `epoch`, `hostPorts`,
  `hasCacheDisk` fields
- **supervisor/init.rs**: new module performing all VM setup in Rust:
  clock sync (libc::clock_settime), virtiofs mounts (libc::mount),
  networking (ip/iptables via Command), cache disk (blkid/mkfs/mount),
  DNS (resolv.conf). All errors logged via tracing → LogSink RPC.
- **Init script**: reduced to minimal mounts (proc/sys/dev/cgroup) + exec
  supervisor. No config reading, no virtiofs, no networking, no cache.
- **libkrun**: built with `make BLK=1` for block device support, added
  `krun_add_disk` FFI call. Removed env var passing (`build_env`).
- **CLI**: passes init config via RPC, computes epoch/shares/cache_disk
  at the caller level.

## 2026-04-06: Gate kernel/assets per platform, add Linux CI and release

Cleaned up the build pipeline so each platform only embeds what it needs.

### Platform-specific assets

- **macOS**: embeds kernel Image + initramfs.gz (for Apple Virtualization)
- **Linux**: embeds rootfs.tar.gz + libkrun.so + libkrunfw.so (no kernel — 
  libkrunfw provides it)
- Rootfs build now produces two formats: gzipped cpio (macOS) and gzipped
  tar (Linux). The tar is extracted at runtime using the `tar` + `flate2`
  crates already in deps.
- `build.rs` and `assets.rs` fully gated with `#[cfg(target_os)]` — no
  placeholder files needed.
- The `initramfs` field in Assets is reused: points to initramfs.gz on
  macOS, extracted rootfs directory on Linux.

### CI pipeline

Added x86_64 and aarch64 Linux build jobs:
- `build-rootfs-x86_64` / `build-libkrun-x86_64` (stage 2)
- `build-rootfs-aarch64` already existed, now uploads both formats
- `build-libkrun-aarch64` (stage 2)
- `build-cli-linux-x86_64` / `build-cli-linux-aarch64` (stage 3)
- Docker builds now `chown` outputs to host UID/GID

### Release and install

- Release workflow packages three variants: darwin-aarch64, linux-x86_64,
  linux-aarch64
- `install.sh` supports Linux + macOS, validates platform combinations,
  uses `sha256sum` on Linux / `shasum` on macOS

## 2026-04-05: Add libkrun VM backend for Linux

First working Linux x86_64 VM support using libkrun + libkrunfw via
dlopen at runtime.

### Approach: dlopen embedded shared libraries

Tried several approaches before landing on this:
1. **krun-sys crate** — requires libkrun.so installed on system
2. **libkrun as Rust dependency** — workspace version conflicts (vm-memory)
3. **staticlib** — duplicate std symbols when linking Rust into Rust
4. **Cloud Hypervisor subprocess** — no built-in virtiofs (needs virtiofsd)

Final approach: build libkrun.so in Docker (isolated workspace), download
libkrunfw.so from GitHub releases, embed both in the binary via
`include_bytes!`, extract to `~/.ezpez/kernel/` at runtime, and load via
`dlopen`. Zero system dependencies — fully self-contained.

### Key implementation details

- **VM backend** (`vm/krun.rs`): loads libkrun + libkrunfw via `dlopen`,
  resolves C API function pointers, configures VM via `krun_set_root` +
  `krun_set_exec("/init")` + `krun_add_virtiofs` + `krun_add_vsock_port2`
- **libkrunfw kernel**: uses libkrunfw's built-in Linux kernel instead of
  our custom kernel, avoiding kernel format issues (libkrun needs ELF,
  our x86_64 build produced bzImage)
- **vsock**: `krun_add_vsock_port2(listen=true)` makes libkrun create a
  host-side UNIX socket. Connect retries handle the race where the host
  connects before the supervisor binds its vsock listener (peek + RST
  detection)
- **RPC transport**: on Linux, vsock maps to a UNIX socket (not TCP),
  so `supervisor.rs` uses `UnixStream` instead of `TcpStream`
- **Init script**: reads config from env vars (`EZPEZ_SHARES`, etc.)
  set via `krun_set_exec`, falling back to kernel cmdline for macOS
- **OCI platform**: fixed hardcoded arm64 resolver to detect host arch
- **Symlinks**: `copy_dir_recursive` now preserves symlinks (Alpine
  rootfs has many)

### Build pipeline

- `mise run build:libkrun` — builds libkrun.so in Docker, downloads
  libkrunfw.so from GitHub releases
- `mise run build:dev` — on Linux, runs build:libkrun before cargo build
- Both .so files embedded in the ez binary (~23MB total)

## 2026-04-06: Fix initial PTY size for container shell

The container shell always started with the wrong terminal size. Three
bugs combined:

1. `crossterm::terminal::size()` returns `(cols, rows)` but the initial
   size was destructured as `(rows, cols)` — rows and cols swapped.

2. The PTY was resized after `spawn()`, but the process had already read
   the default size. Fixed by resizing before spawn.

3. Even with the outer PTY sized correctly, crun creates its own PTY for
   the container. Without `consoleSize` in the OCI config.json, crun
   uses the kernel default. Added `consoleSize` (height/width) to the
   OCI spec so crun sets the container PTY correctly at creation.

Added debug logging at all levels: host initial size, host resize,
supervisor initial PTY size, supervisor PTY resize events.

## 2026-04-05: Add mount `missing` action and resolve relative paths

Mount source paths are now resolved against the project's cwd before
checking existence, fixing `./target`-style relative paths. Added a
configurable `missing` field to mount config controlling behavior when
the source doesn't exist:

- `fail` (default) — error out
- `warn` — skip with a warning message
- `ignore` — skip silently
- `create` — create the directory and mount it

18 tests covering: absolute/relative/tilde paths, all missing actions,
nested create, mixed mounts, file vs dir detection, read_only flag.

## 2026-04-05: Add comprehensive network tests

33 end-to-end tests covering TCP relay, HTTP proxy, TLS MITM, TLS
passthrough, ALPN negotiation, and Lua middleware.

### Test infrastructure

- `run_network` / `run_with_config` — starts a full capnp-rpc system
  (client + server VatNetwork over tokio DuplexStream) on a LocalSet
- `TestConnection` — simulates the supervisor side, sends/receives bytes
  through the RPC channel
- `RpcStream` — `AsyncRead + AsyncWrite` adapter over the RPC channel
  for TLS client handshakes in tests
- `serve()` — axum HTTP server on random port
- `serve_https()` — HTTPS server with test CA + leaf cert
- `RequestLog` — captures Lua `log()` calls for test assertions
- `LogFn` — configurable log sink (tracing in production, collector in tests)

### Test coverage

- **TCP** (7): plain HTTP, host allowlist (deny, wildcard, star, empty),
  POST with body, large response
- **HTTP proxy** (5): detection with middleware, raw relay without
  middleware, POST through proxy, status codes, response headers
- **Middleware** (14): deny by path/host, inject headers, read/replace
  request body, JSON body coercion, explicit send + response inspection,
  modify response status/headers/body, implicit send, multiple layers,
  JSON response parsing, body length
- **TLS** (7): MITM basic, passthrough, MITM with middleware, ALPN
  h1↔h1, h1 when server offers h2+h1, no ALPN fallback, h2↔h2

### Bug fixes from tests

- `is_denied()` now checks `CallbackError` nesting (deny from Lua was
  wrapped in `CallbackError`, not matched as `ExternalError`)
- `setBody()` updates Content-Length header automatically
- Host getter strips port from Host header for `hostMatches()`

## 2026-04-05: Add middleware-style HTTP scripting with body/response support

Reworked the Lua scripting engine from a simple request filter to a full
middleware pipeline with request + response interception.

### Middleware architecture

Scripts compose as layers around the actual HTTP send, like web framework
middleware. Each script receives `req` as a function parameter (not a
global — prevents races on concurrent requests). Scripts can:

- Inspect/modify request (method, path, headers, body)
- Call `req:send()` to forward and get the response (async, yields)
- Inspect/modify response (status, headers, body)
- Call `req:deny()` to reject the request
- If `send()` is not called, it's called implicitly after script ends

### Body userdata

New `Body` type wrapping `Bytes` with:
- `body:text()` — raw bytes as Lua string
- `body:json()` — parse JSON → Lua table (via mlua serde)
- `#body` — byte length
- `FromLua` coercion: string → bytes, table → JSON, Body → clone, nil → empty
- `req:setBody()` / `res:setBody()` accept any coercible value

### Implementation

- `State` wraps `Rc<RefCell<Option<(Parts, Body)>>>` — Lua field
  getters/setters read/write hyper request parts directly
- `RespState` wraps `Rc<RefCell<Option<Response>>>` — same for response
- `with_req()`/`with_resp()` helpers eliminate Option unwrap boilerplate
- `header(key)` / `setHeader(key, val)` for single-header access
- Scripts wrapped in `function(req)..end` to make `req` a local parameter
- `CompiledMiddleware` uses `Rc<Inner>` for cloneability across requests
- mlua `async` + `serialize` features for async methods + JSON support

### Config rename

`[[network.rules]]` → `[[network.middleware]]` to reflect the new role.

## 2026-04-05: Move TLS interception from supervisor to CLI

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
## 2026-04-05: Add x86_64/Linux build support, convert to mise file tasks

Preparing for Linux host support by making the build pipeline
architecture-aware and converting build tasks to mise file tasks.

### Build tasks → file tasks

Moved all build tasks from inline `mise.toml` definitions to standalone
scripts in `mise/tasks/build/`. File tasks are easier to maintain for
multi-line scripts and allow proper shell tooling (shellcheck, editor
support). Non-build tasks (test, format, lint, ez) remain inline since
they're one-liners.

### Architecture detection

- **Kernel**: `build.sh` now receives `ARCH` env var. On x86_64 hosts
  it uses `config-x86_64` and copies `bzImage`; on arm64 it keeps the
  existing `config-arm64` and copies `Image`. Output is always
  `sandbox/out/Image` regardless of arch.
- **Supervisor**: Detects `uname -m` to pick the right musl target
  (`x86_64-unknown-linux-musl` vs `aarch64-unknown-linux-musl`).
  Defaults to Docker build on all platforms; set
  `SUPERVISOR_BUILD_HOST=true` for native toolchain (used in CI where
  deps are pre-installed).
- **Dev CLI**: Skips macOS codesign on Linux.
- **x86_64 kernel config**: New `config-x86_64` mirrors the arm64
  config (namespaces, cgroups, virtio, vsock, netfilter, virtiofs) with
  x86-specific settings (KVM_GUEST, PARAVIRT, 8250 serial).

### CI workflow

Replaced hardcoded supervisor build commands in CI with
`SUPERVISOR_BUILD_HOST=true mise run build:supervisor`.

### Lint fixes

Added `#[cfg(target_os = "macos")]` gate on `CString` import and
`#[allow]` annotations for `VmConfig` fields and `vm::start` async —
these are unused on Linux until a VM backend is added.

## 2026-04-05: Grant all capabilities, fix CA certs

Three fixes to make containers work properly:

1. **All Linux capabilities**: crun drops capabilities by default. Tools
   like `apt-get` need `CAP_SETUID`/`CAP_SETGID` to drop privileges.
   Since the VM is the security boundary, grant all capabilities in the
   OCI spec — no reason to restrict inside the VM.

2. **CA cert paths**: The MITM CA cert was only installed at the Debian
   path (`/etc/ssl/certs/ca-certificates.crt`). Alpine's LibreSSL reads
   `/etc/ssl/cert.pem`. Now writes to all common distro paths.

## 2026-04-05: Add cache volume via VirtIO block device

VirtioFS mounts have significant overhead for metadata-heavy operations
(builds, package managers). A VirtIO block device with ext4 provides
native filesystem performance inside the VM for cache data.

### Config

```toml
[cache]
size = "20 GB"
mounts = ["~/.cache", "/cache"]
```

The `[cache]` section is fully optional — omitting it means no cache.
Removing it after use deletes the disk image. Size uses smart-config's
built-in `ByteSize` type (parses "20 GB", "512 MB", "4 KiB", etc.),
which also replaced the old `memory_mb: u64` with `memory: ByteSize`.

### Disk image lifecycle

- **Create**: sparse raw file via `File::set_len()` (no actual disk use)
- **Grow**: `set_len()` to new size + `resize2fs` in init expands fs
- **Shrink**: delete + recreate (ext4 reformatted on next boot)
- **Remove**: deleting `[cache]` from config removes `cache.img`

### Architecture

Host CLI creates a sparse raw disk image and attaches it as a VirtIO
block device via `VZDiskImageStorageDeviceAttachment` +
`VZVirtioBlockDeviceConfiguration`. The init script formats ext4 on
first use and mounts at `/mnt/cache`. Cache mount subdirs use the
container target path (without leading `/`) so reordering mounts in
config doesn't mix up data. Subdirs are created by the supervisor
via a new `cacheDirs` RPC field, keeping init simple.

### Files changed

- `cli/src/config.rs` — `Cache` struct, `ByteSize` for memory+cache
- `cli/src/oci/cache.rs` — new: disk image management + mount resolution
- `cli/src/oci.rs` — `MountType::Cache`, `Bundle.cache_image`, `cache_dirs()`
- `cli/src/vm/config.rs` — `cache_disk: Option<PathBuf>`
- `cli/src/vm/apple.rs` — VirtIO block device attachment
- `cli/src/vm.rs` — skip VirtioFS for cache, pass cache_disk, display
- `cli/Cargo.toml` — objc2-virtualization block device features
- `protocol/schema/supervisor.capnp` — `cacheDirs` in start RPC
- `cli/src/rpc/supervisor.rs` — send cacheDirs
- `sandbox/supervisor/src/rpc.rs` — receive cacheDirs
- `sandbox/supervisor/src/main.rs` — create cache subdirs
- `sandbox/rootfs/init` — ext4 format, mount, resize2fs
- `sandbox/rootfs/build.sh` — e2fsprogs package

## 2026-04-04: Prompt before download and GC unused images

Moved the image-change prompt to before layer download so "keep old
environment" skips downloading entirely. When user re-creates an
environment, `gc_unused_image` scans all project digest files — if
no project references the old image, the cached image is deleted.

## 2026-04-04: Simplify network filtering to host allowlist

Replaced `default_mode` + `tcp_connect` Lua scripts with a simple
`allowed_hosts` pattern list. Empty list = deny all traffic. Supports
`*` (allow all), exact match, and `*.domain.com` wildcards.

Removed: `NetworkMode` enum, `NetworkRuleType` enum, `tcp_connect`
script type, `ConnectRequest` Lua userdata. The TCP connect step now
just checks `is_host_allowed()` — no Lua involved.

HTTP request scripts simplified: requests are allowed by default,
scripts can only deny (no explicit `req:allow()` needed).

Renamed `allowed_hosts_tls` to `tls_passthrough` for clarity.

## 2026-04-04: TLS passthrough for cert-pinned hosts

Added `allowed_hosts_tls` config (supports globs like `*.example.com`)
for hosts whose TLS should not be intercepted. Supports tools with
certificate pinning that reject the MITM CA.

Flow: passthrough list sent to supervisor via RPC `tlsPassthrough`.
Supervisor proxy checks hostname against the list — if matched, skips
TLS MITM and forwards raw TLS bytes. Tells CLI `tls=false` so CLI
doesn't wrap with its own TLS. CLI also skips HTTP interception for
passthrough hosts (`http_engine = None`). Result: end-to-end TLS
between container and real server, no MITM, no HTTP parsing.

## 2026-04-04: Refactor mounts, add tilde expansion, move cmd to RPC

### Mount resolution refactoring

Separated mount resolution from execution:

- `oci::prepare` resolves everything: tilde expansion on source
  (host `~`) and target (container `~` via rootfs `/etc/passwd`),
  mount type detection (`Dir`/`File`), OCI config.json generation.
  Returns `Bundle` with `Vec<ResolvedMount>`.
- `vm::start` reads `bundle.mounts` to add VirtioFS shares, hardlink
  files, and build the kernel cmdline. No more `PreparedMounts` /
  `mounts.rs` — removed in favor of direct mount handling in `vm.rs`.
- `ResolvedMount` carries display paths (original with `~`) separate
  from resolved paths, plus `MountType::Dir { key }` / `File { filename }`
  with `key()` and `vm_path()` helpers.

Fixed bug: OCI config.json was written before config mounts were
added, so crun never saw user mounts. Now all mounts are assembled
before `generate_config`.

### Supervisor command via RPC

Added `cmd :Text` and `args :List(Text)` to the supervisor start
RPC. CLI builds the crun command and sends it — supervisor just
executes whatever it receives. Added `dev` cargo feature: with
`EZ_DEV_NO_CRUN=true` env var, sends `/bin/sh` instead of crun
for debugging the VM without the container.

## 2026-04-04: Centralize workspace dependencies and update rcgen

Lifted all dependencies (except CLI macOS target deps) to
`[workspace.dependencies]` so versions are managed in one place.
Crates reference them with `{ workspace = true }`, adding features
locally where needed.

Updated rcgen from 0.13 to 0.14: `CertificateParams::from_ca_cert_pem`
moved to `Issuer::from_ca_cert_pem`, and `signed_by` now takes an
`&Issuer` instead of separate cert + key args. Updated crossterm to
0.29.

## 2026-04-04: CLI console improvements and robustness

### Console module (`cli.rs`)

Merged `console.rs` into `cli.rs`. Renamed `Cli` struct to `CliArgs`
to avoid confusion with the module name. All console utilities now
accessed as `cli::log!`, `cli::check()`, `cli::dim()`, etc.

Added:

- `cli::error!` macro for red error output
- `cli::check()` green checkmark, `cli::bullet()` for detail lines
- `cli::dim()` grey text for secondary values (digests, sizes, etc.)
- `cli::red()` for error messages
- `cli::interrupted()` / `cli::is_interrupted()` via `watch` channel
  (race-free signal handling for Ctrl+C during downloads)
- `cli::is_interactive()` for interactive prompts
- Progress bars with byte-level updates via `ProgressWriter`
- Image change prompt (re-create/continue/cancel) via dialoguer

### Download robustness

- Raw terminal mode deferred until VM boot — Ctrl+C works during
  downloads
- Downloads write to `.tmp` files, renamed atomically on success
- Layer size verified after download, corrupt files re-downloaded
- `.tmp` cleanup on startup for interrupted downloads
- `tokio::select!` races each download against `interrupted()`

### Bundle consistency

Digest file is the atomicity marker — written last after successful
rootfs copy. Missing = incomplete (clean up and recreate). Mismatched
= image changed (prompt user).

### Project locking

PID-based lockfile prevents concurrent instances from modifying the
same project. Stale locks (dead PID) are taken over. Atomic via
write-to-tmp + rename + verify pattern. Released on drop.

### Error handling

Removed `CliError` enum — all errors use `anyhow::Result` now.
Top-level errors printed in red.

### Logging

Replaced `verbose :Bool` with `logFilter :Text` in RPC schema.
Both CLI and supervisor use the same `EnvFilter` string. Configurable
via `--log-level` flag (trace/debug/info/warn/error).

## 2026-04-04: Fix HTTP proxy URI and port handling

The HTTP proxy was sending absolute URIs (`GET http://host:port/path`)
to h1 origin servers. HTTP/1.1 origin servers expect relative paths
(`GET /path`) with the authority in the Host header — absolute URIs
are only for forward proxies. This caused Python SimpleHTTPServer
(and likely other servers) to 404 on every request.

Also fixed missing port in the outgoing URI for h2.

Now: h1 uses relative path, h2 uses absolute URI (needed for
`:authority`/`:scheme` pseudo-headers). Protocol is passed through
from ALPN detection to the request handler.

## 2026-04-04: Enable clippy pedantic workspace-wide

Added `[workspace.lints.clippy]` with `pedantic = "deny"` and
selective allows for noisy rules. All three crates inherit via
`[lints] workspace = true`. Fixed all pedantic violations across
the workspace:

- Replaced raw pointer casts with `(&raw const ..).cast::<T>()`
- Fixed RefCell borrows held across await points (clone before await)
- Replaced `Default::default()` with explicit type defaults
- Combined identical match arms
- Collapsed nested if statements
- Replaced redundant closures with function pointers
- Changed `&PathBuf` params to `&Path`
- Boxed large enum variants
- Used `strip_prefix` instead of manual prefix stripping
- Added targeted `#[allow]` for unavoidable cases (large futures,
  module inception, too many arguments, await holding refcell)

Also added `mise lint` task (fmt check + clippy) and `mise format`.

## 2026-04-04: HTTP/2 support + RefCell soundness fix

### HTTP/2 support

Added end-to-end HTTP/2 support for the proxy:

- Supervisor MITM TLS config advertises h2+http/1.1 via ALPN, so
  containers can negotiate HTTP/2 during TLS handshake
- CLI TLS client config also advertises h2+http/1.1 ALPN to real
  servers
- CLI checks ALPN result from real server connection to determine
  protocol: h2 → `hyper::client::conn::http2`, h1 → http1 client
- `RequestSender` trait abstracts over h1/h2 clients so the request
  handler works with either
- `LocalExec` executor spawns h2 stream tasks via `spawn_local`
- Outgoing request uses absolute URI (`https://host/path`) so hyper
  correctly derives h2 `:authority`/`:scheme` pseudo-headers

### HTTP detection improvement

Replaced 4-byte prefix check with proper request line parsing:
buffer up to 4KB or first `\r\n`, then validate with regex
`^[A-Z]+ \S+ HTTP/\S+$` (h1) or `^PRI \* HTTP/2\.0$` (h2 preface).

### RefCell soundness fix

Audited all RefCell usage in the network module:

- **ChannelSink::tx** — was holding `Ref` across `.await` in `send()`,
  which could panic if `close()` was dispatched concurrently by
  capnp-rpc. Fixed by cloning the sender before awaiting.
- **RequestSender** — `borrow_mut()` drops before `.await` (sound)
- **RelayError** — writer sets after loop ends (sound)

## 2026-04-03: HTTP request interception + logging improvements

### HTTP interception via hyper

Added `http_request` rule type that intercepts HTTP traffic at the
application layer. When http_request rules are configured, the relay
detects HTTP by peeking at the first bytes, then hands off to hyper.

Architecture: hyper server parses requests from the container side
(via `RpcTransport` bridging the RPC byte channel into AsyncRead/
AsyncWrite), while hyper client forwards to the real server (via
`CombinedStream` reuniting the split read/write halves). Bodies
stream through without buffering.

Per-request flow:

1. hyper server parses `Request<Incoming>` from container
2. Extract method/path/headers, run http_request Lua scripts
3. Scripts can modify headers (e.g., inject auth tokens) and path
4. If denied → return 403 to container via hyper
5. If allowed → build modified request, `sender.send_request()` to
   real server, stream `Incoming` response back

Keep-alive handled automatically by hyper's `serve_connection`.
Falls back to raw byte relay if first bytes aren't HTTP.

### Relay error propagation

Shared `RelayError` cell (`Rc<RefCell<Option<String>>>`) between the
relay task and `ChannelSink`. When the relay fails, it writes the
error; `ChannelSink::send()` checks and returns it to the supervisor
so the container's connection is closed with a meaningful error.

### Logging improvements

- CLI tracing writes to `ez.log` (no ANSI colors) instead of stderr
  to avoid interfering with shell stdout
- Verbose mode uses target filtering: `warn,ez=debug` on CLI,
  `ezpez_supervisor` prefix filtering on supervisor
- Verbose flag passed to supervisor via RPC `start(verbose :Bool)`
- Error levels: vsock/RPC failures → `error!`, remote server/VM app
  events → `debug!`, fine-grained flow → `trace!`

## 2026-04-03: Lua network scripting + RPC deny protocol

### What

Scriptable network filtering via inline Lua (LuaJIT) rules in config.
Each outbound connection runs through all tcp_connect rules. Deny
short-circuits; allow must be explicit or fall back to default_mode.

### Rule configuration

Rules defined as `[[network.rules]]` in ez.local.toml with name, type
(tcp_connect/http_request enum), required env vars with descriptions,
and inline Lua script. Env vars are validated at startup (hard error
if missing) and snapshotted into per-rule `env` table.

### Lua sandbox

Each rule gets its own Lua VM (mlua + LuaJIT). Dangerous globals
removed (os, io, debug, load, require). Instruction limit via hook
(1M instructions). Scripts compiled once via `into_function()`, called
per connection.

### Lua API

- `req.host/port/tls` — read/write (tls read-only)
- `req:allow()`, `req:deny()` — set permission
- `req:hostMatches(pattern)` — glob match (e.g. "*.github.com")
- `env.VAR_NAME` — declared env vars only
- `log(msg)` — tracing debug output

### Permission model

1. All tcp_connect rules execute in order
2. If any calls `req:deny()` → denied immediately
3. After all rules: if any called `req:allow()` → allowed
4. Otherwise: use `network.default_mode` (allow/deny, default deny)

Initial `allowed` state matches default_mode, so with `allow` mode
scripts only need to deny.

### RPC protocol extension

`NetworkProxy.connect` now returns `ConnectResult` union (server/denied)
instead of bare TcpSink. Supervisor proxy handles denied responses by
shutting down the container's connection cleanly with a debug log.

### CLI refactoring

Module `mod.rs` files converted to `<module>.rs` where possible.
Network scripting under `network/scripting/` with `connect_request.rs`
as separate userdata type (extensible for future http_request type).

## 2026-04-03: Hierarchical TOML configuration

### What

Replaced hardcoded `Config::default()` with hierarchical TOML config
file loading via smart-config. CLI flags (`--verbose`, `-- args`)
remain as runtime overrides.

### Config file loading

Files loaded in order (later merges over former):

1. `~/.ezpez/config.toml` — global defaults
2. `~/.ez.toml` — user-level shorthand
3. `<project-root>/ez.toml` — project config (committed)
4. `<project-root>/ez.local.toml` — local overrides (gitignored)

Merge rules: arrays concatenate, objects merge recursively,
primitives and type mismatches override with the latter value.

### smart-config integration

Config structs use `DescribeConfig` + `DeserializeConfig` derives for
schema-driven validation with rich error messages. Custom `Nested<T>`
deserializer bridges `DeserializeConfig` types inside `Vec<T>` by
deserializing the raw JSON object first, then feeding it into a fresh
ConfigSchema for full error collection. Thread-local nesting depth
tracks indentation for nested error formatting.

### Defaults

- `cpus`: all available cores (`std::thread::available_parallelism`)
- `memory_mb`: half of system RAM (via `sysinfo` crate)
- `image`: `alpine:latest`
- `verbose` moved to CLI-only flag (not in config files)

### CLI refactoring

- `assets/mod.rs` → `assets.rs` (no sub-modules)
- Fixed `Rc` import in `network/server.rs` (`std::rc::Rc`)
- `verbose` removed from Config, passed directly to `vm.start()`

## 2026-04-03: Virtual DNS for container networking

### What

Virtual DNS server in the supervisor assigns synthetic IPs (10.2.0.0/16)
to hostnames. The proxy reverse-lookups these IPs to recover the
hostname, giving full hostname visibility for all connections including
plain HTTP.

### Virtual DNS design

Instead of forwarding real DNS queries, the supervisor runs a minimal
UDP DNS server on 10.0.0.1:53. When a DNS A query comes in (e.g., for
`example.com`), it allocates a virtual IP from the 10.2.0.0/16 block
and caches the bidirectional mapping. The container's `/etc/resolv.conf`
points to `nameserver 10.0.0.1`.

When the container connects to the virtual IP, iptables redirects to
the proxy. The proxy gets the original destination via SO_ORIGINAL_DST,
reverse-lookups the virtual IP → hostname, and forwards to the CLI
with the real hostname. The CLI resolves the hostname for real.

Uses `simple-dns` crate for DNS wire format and `scc::HashMap` for
lock-free concurrent lookups (shared between DNS server and proxy).

### TLS MITM trust

The project CA cert is installed into the container's trust store
during bundle preparation: read from the pristine image rootfs,
append the CA cert, write to the bundle rootfs. This makes curl,
wget, etc. trust the MITM certificates. Reading from the image
(not bundle) avoids duplicating the CA on repeated runs.

### VM clock

The VM has no RTC, so the system clock starts at epoch. The host's
current time is passed via kernel cmdline (`ezpez.epoch=<seconds>`)
and set in the init script with `date -s`.

### CLI refactoring

Separated network concerns: `cli/src/network/` module owns the TLS
config (`Arc<ClientConfig>` built once) and host_ports filtering.
Uses `rustls-native-certs` for the macOS system certificate store
instead of bundled `webpki-roots`.

## 2026-04-03: Host port forwarding, entrypoint args, pipe mode

### Host port forwarding

Expose configured host ports (`config.network.host_ports`) to the VM.
Per-port iptables REDIRECT rules forward specific localhost ports to
the host via the proxy, while other localhost traffic passes through
directly so local VM services work. Host ports are passed via kernel
cmdline (`ezpez.host_ports=9999,8080`) and enforced both in iptables
and in the CLI's NetworkProxyImpl.

### Entrypoint args

`ez -- <args>` overrides the entire OCI command. User args replace
the image's entrypoint+cmd entirely (e.g., `ez -- ls /usr`). When
no args are provided, falls back to image entrypoint+cmd or `/bin/sh`.

### Pipe mode (no PTY)

When stdin is not a TTY (piped input), the VM runs without PTY:

- OCI config sets `"terminal": false`
- Supervisor spawns process with piped stdin/stdout/stderr instead of
  PTY, giving proper separation of stdout and stderr
- CLI skips raw terminal mode and resize handling
- StdinImpl handles optional resize signal (None in pipe mode)

This enables: `echo data | ez -- grep pattern`,
`ez -- sh -c 'echo hi; exit 42'` (exit codes propagate).

## 2026-04-03: Expose host ports to the VM

### What

Allow the VM to connect to specific ports on the host machine via the
transparent proxy. Configured via `config.network.host_ports` (default:
`[9999]`).

### Design

The challenge: iptables REDIRECT catches ALL outbound TCP. If a service
runs inside the VM on port 8080, connecting to `localhost:8080` would
loop through the proxy and try to reach the *host's* port 8080 instead.

The fix uses per-port iptables rules:

1. Host ports (from config): `localhost:<port>` → REDIRECT to proxy →
   RPC → host's localhost
2. Other localhost: RETURN (bypass proxy, local services work directly)
3. External traffic: REDIRECT to proxy → RPC → host makes connection

Host ports are passed to the VM via kernel cmdline (`ezpez.host_ports=
9999,8080`). The init script parses them and generates per-port REDIRECT
rules before the general localhost RETURN rule.

On the CLI side, `NetworkProxyImpl` enforces the same allowlist: only
ports listed in `host_ports` are permitted for localhost connections.
This provides defense-in-depth (iptables in VM + RPC filtering in CLI).

## 2026-04-03: Forward host signals to VM + supervisor tracing via RPC

### What

Forward signals received by the CLI (SIGHUP, SIGINT, SIGQUIT, SIGTERM,
SIGUSR1, SIGUSR2) to the container process in the VM. Route supervisor
tracing output through the LogSink RPC so it appears in the CLI.

### Signal forwarding

Signals are captured on the CLI via `async_stream` + `tokio::select!`
over all forwardable signal kinds, yielding Linux signal numbers
(hardcoded constants, not `libc::` — the target is always the Linux VM
regardless of the macOS host where SIGUSR1/2 differ).

The `Process.signal(signum :Int32)` RPC delivers signals to the
supervisor. For SIGINT and SIGQUIT, the supervisor writes the
corresponding PTY control character (`\x03`, `\x1c`) to the PTY
master — this goes through the terminal discipline and works across
PID namespaces, exactly like Ctrl+C. For other signals, it falls
back to `kill(-pid, sig)` on crun's process group.

The PTY control character approach was chosen because:

- crun runs the container in a PID namespace
- The shell has job control disabled ("can't access tty")
- `tcgetpgrp()` returns crun's PGID, not the shell's
- `kill()` to crun's process group doesn't reach container processes
- But writing `\x03` to the PTY master works (same as Ctrl+C)

### Supervisor tracing via LogSink RPC

Added a tracing subscriber layer to the supervisor that forwards all
`tracing::*!()` events through the LogSink RPC to the CLI. The CLI's
LogSinkImpl now uses `tracing::debug!/info!/warn!/error!` with
`target: "vm"` so supervisor messages go through the CLI's tracing
subscriber and respect `--verbose`.

Removed the manual `Logger` struct from the network proxy in favor of
standard tracing macros.

## 2026-04-03: Merge resize into Stdin RPC + supervisor refactor

### What

Replaced ByteStream with a dedicated Stdin RPC interface that carries
both keyboard data and terminal resize events in a single stream.
Refactored supervisor process management to use channel-based
communication between the main loop and the RPC ProcessImpl.

### Why

Having `resize` as a separate method on `Process` required splitting
the PTY into read/write halves and sharing the writer via
`Rc<RefCell<Option<OwnedWritePty>>>` between the stdin relay task and
resize handler. This interior mutability caused problems when resize
was called while stdin relay was writing.

By merging resize into the stdin stream (`ProcessInput` union of
`stdin: DataFrame | resize: TermSize`), the PTY writer stays in a
single task that reads from the Stdin RPC and dispatches both data
writes and resize calls — no sharing needed.

### Design

**Schema changes:**

- Removed `ByteStream` interface entirely
- Added `Stdin { read() -> ProcessInput }` interface
- Removed `resize` from `Process` (now poll/signal/kill only)
- Removed `err` variant from `DataFrame` (just eof/data)
- `Supervisor.start()` takes `Stdin` instead of `ByteStream`

**CLI side:**

- New `StdinImpl` (stdin::Server) multiplexes tokio stdin reads and
  SIGWINCH resize signals via `tokio::select!` in a single `read()`
- Removed separate resize loop from main — resize events flow through
  the Stdin capability automatically
- Removed `OutputStream`/`InputStream` stream abstractions from
  protocol crate (no longer needed without ByteStream)

**Supervisor side:**

- `ProcessImpl` now communicates with `attach()` via channels:
    - `frames` channel: PTY output + exit code → ProcessImpl.poll()
    - `signals` channel: ProcessImpl.signal()/kill() → attach() loop
- `attach()` owns the main select loop (PTY reads + signal dispatch)
- `relay_stdin()` extracted as standalone async fn
- PTY read errors (EIO on child exit) break the loop correctly
- Supervisor hangs after sending exit frame; CLI kills VM to stop it

**Build:**

- Added Docker volume for supervisor target dir to cache incremental
  builds across `mise run build:supervisor` invocations

## 2026-04-02: WIP — Transparent network proxy via vsock

### What

Infrastructure for transparent TCP proxy: iptables redirect in the VM,
TLS MITM with per-project CA, push-based TcpSink RPC for bidirectional
relay, CLI-side NetworkProxy that makes real outbound connections.

### Status

- Kernel: netfilter/iptables/NAT modules enabled ✓
- Init: iptables REDIRECT + dummy default route ✓
- Supervisor: proxy listener on port 8080 ✓
- Supervisor: TLS interceptor (rcgen + rustls) ✓
- RPC: NetworkProxy + TcpSink push-based schema ✓
- CLI: NetworkProxy impl with real TCP + TLS ✓
- CA: auto-generated per project, mounted into VM ✓
- **BUG**: Relay chain hangs — proxy accepts connections but
  data doesn't flow through the RPC relay to the host. Needs
  debugging of the TcpSink bidirectional relay.

## 2026-04-02: Add directory and file mounting via VirtioFS

### What

Mount the project directory and configurable paths into the container
via VirtioFS shares + bind mounts. Container shell starts in the
project directory (same absolute path as host).

### How it works

1. CLI collects mounts: project CWD (always) + config.mounts
2. Each directory becomes a VirtioFS share with a unique tag
3. File mounts: hard-linked into `~/.ezpez/projects/<hash>/files_{rw,ro}/`,
   shared as a VirtioFS directory. Bidirectional sync via hard links.
   Falls back to copy (with warning) on cross-device.
4. VirtioFS tags passed to guest via kernel cmdline (`ezpez.shares=...`)
5. Init script parses cmdline, mounts each tag at `/mnt/<tag>`
6. config.json bind-mounts from `/mnt/<tag>` into container paths

### Mount order (latter shadows former)

1. Project directory (CWD → same absolute path in container)
2. Config mounts in definition order

### Key decisions

- **One VirtioFS device per share** (not VZMultipleDirectoryShare) —
  simpler, and each share can independently be read-only or read-write.
- **Hard-link for file mounts** — same inode = bidirectional sync.
  Separate `files_rw/` and `files_ro/` directories since VirtioFS
  share read-only is set at the share level.
- **Tags via kernel cmdline** — `/sys/fs/virtiofs/*/tag` sysfs
  attribute doesn't exist in our kernel config. Cmdline is reliable.
- **Asset cache invalidation** — compare embedded binary size with
  cached file size to detect binary updates. Fixes stale initramfs
  after supervisor rebuild.
- **Container CWD = host CWD** — config.json `process.cwd` set to
  the canonical project path so the shell starts in the project dir.

## 2026-04-01: Pull OCI images from Docker Hub with caching

### What

Replace the static Alpine minirootfs bundle with real OCI image
pulling from Docker Hub. Images are cached per-digest, project
bundles use APFS copy-on-write for persistent state across sessions.

### Cache layout

```
~/.ezpez/
  kernel/Image, initramfs.gz     # extracted once from embedded assets
  images/<digest>/rootfs/        # downloaded + extracted image layers
  projects/<hash>/bundle/        # CoW copy of image rootfs + config.json
```

### New modules

- `cli/src/oci/registry.rs` — `oci-client` wrapper: resolve image
  ref → manifest + config + digest, pull layer blobs. Uses
  `linux/arm64` platform resolver.
- `cli/src/oci/layer.rs` — extract tar.gz layers in order into
  merged rootfs. Handles OCI whiteout files (.wh.*) for deletions.
- `cli/src/oci/config.rs` — generate OCI runtime config.json from
  image config (CMD, ENTRYPOINT, ENV, WorkingDir, User).
- `cli/src/oci/cache.rs` — cache directory management, APFS
  `clonefile` CoW copy with fallback to regular copy.
- `cli/src/project.rs` — project hash from canonical CWD.
- `cli/src/assets/mod.rs` — refactored to cache-based (no more
  tempfile, kernel/initramfs persist in `~/.ezpez/kernel/`).

### Key decisions

- **`oci-client` crate** for registry interaction — async, handles
  auth, manifests, blob downloads. Platform resolver set to
  `linux/arm64` since the VM is ARM64.
- **Project hash = sha256(canonical_cwd)** — image digest stored
  separately as a file for change detection. Different directories
  get independent persistent state.
- **APFS clonefile** for project bundle copies — instant CoW on
  APFS, fallback to regular copy on other filesystems.
- **Derive config.json from image** — CMD, ENTRYPOINT, ENV,
  WorkingDir, User read from image config. Fallback to /bin/sh
  if none specified.
- **Removed mise build:bundle task** — bundle preparation is now
  runtime (on first `ez` run), not build time.

## 2026-04-01: Refactor CLI and supervisor into clean modules

### What

Split the monolithic main.rs files into focused modules. Introduced
anyhow for error handling. Added protocol stream wrappers
(`OutputStream`/`InputStream`) with standard Rust async traits.

### CLI structure

```
cli/src/
  main.rs        — 50 lines: parse args, create config, run, handle exit
  config.rs      — Config struct with defaults
  error.rs       — CliError { Expected, Unexpected(anyhow) }
  terminal/      — TerminalGuard + SIGWINCH resize signal
  vm/            — vm::create(config) → (VmHandle, OwnedFd)
  rpc/
    client.rs    — Client: connect, exec
    process.rs   — Process: poll, resize; ProcessEvent enum
```

### Supervisor structure

```
sandbox/supervisor/src/
  main.rs        — 20 lines: vsock listen, serve
  vsock.rs       — OwnedFd-based listen/accept
  rpc/
    server.rs    — SupervisorImpl, serve()
    process.rs   — spawn(), ProcessImpl
```

### Protocol streams (`protocol/src/streams.rs`)

- `OutputStream`: boxed `dyn AsyncRead` → `byte_stream::Client` via
  `From` conversions. No generic param.
- `InputStream`: `byte_stream::Client` → `impl AsyncRead`. Stateful
  poll-based implementation that properly persists pending RPC futures
  between polls. Internal buffer for partial reads.

### Key decisions

- **anyhow for supervisor** — all errors are unexpected, no need for
  typed error enum.
- **CliError { Expected, Unexpected }** — Expected errors show clean
  messages, Unexpected use anyhow's `{:#}` formatting with context.
- **`LocalSet::run_until()`** not `enter()` — `enter()` only sets
  spawn context but doesn't drive tasks. `run_until()` both sets
  context AND polls spawned tasks, required for capnp-rpc's
  `spawn_local` to work.
- **`InputStream` implements `AsyncRead`** not `Stream` — more
  fundamental trait, `Stream` can be derived via
  `tokio_util::io::ReaderStream` if needed later.

## 2026-04-01: Run Alpine container via crun + VirtioFS

### What

The shell now runs inside an OCI container (Alpine) managed by `crun`,
with the container bundle prepared on the host and shared into the VM
via VirtioFS.

### Architecture

```
Host: .tmp/bundle/ ──VirtioFS──→ VM: /mnt/bundle
  config.json                      crun run --no-pivot --bundle /mnt/bundle
  rootfs/ (Alpine minirootfs)      → containerized /bin/sh with PID namespace
```

### Changes

- **Kernel config**: enabled namespaces (PID, UTS, NET, USER, IPC,
  mount) and cgroups (pids, memory, cpu, freezer, devices). Required
  by crun for container isolation.
- **Rootfs**: added `crun` package, cgroup2 mount, VirtioFS mount
  at `/mnt/bundle`.
- **VirtioFS**: `VZVirtioFileSystemDeviceConfiguration` shares the
  host bundle directory into the VM with tag `"bundle"`.
- **Bundle**: mise task `build:bundle` downloads Alpine minirootfs,
  creates OCI runtime bundle at `.tmp/bundle/` with `config.json`.
- **Supervisor**: `exec` now runs `crun run --no-pivot --bundle
  /mnt/bundle ezpez0` instead of `/bin/sh`.

### Key decisions

- **`--no-pivot`** — VirtioFS (FUSE-based) doesn't support the
  `pivot_root` syscall. `--no-pivot` uses `chroot` instead, which is
  fine since the VM is the real security boundary, not the container.
- **`terminal: true`** in config.json — crun creates a PTY for the
  container process, fixing the "can't access tty" warning.
- **Hard-coded Alpine** — MVP uses a fixed Alpine minirootfs. Dynamic
  image pulling (via `oci-client` crate) is a future step.
- **No IPC namespace** — kernel's `CONFIG_IPC_NS` requires
  `CONFIG_SYSVIPC` which wasn't enabled. Omitted from config.json;
  not essential for MVP.
- **Hand-written config.json** — static 40-line OCI spec. No
  `oci-spec` crate needed for a fixed config.

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
