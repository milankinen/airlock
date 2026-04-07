# Add libkrun VM backend for Linux

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
