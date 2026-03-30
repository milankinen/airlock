# ezpez MVP Implementation Plan

## Context

ezpez is a greenfield Rust CLI tool for running untrusted code in lightweight VMs. The MVP goal is: **boot a Linux VM on macOS and give the user an interactive shell** -- no container runtime, no networking, no config files yet. This establishes the foundational VM layer that everything else builds on.

## Architecture

```
┌──────────────────────────────────────────────┐
│  CLI (clap)                                  │
│  Parses args, orchestrates lifecycle         │
├──────────────────────────────────────────────┤
│  Terminal I/O (crossterm)                    │
│  Raw mode, stdin/stdout relay, cleanup guard │
├──────────────────────────────────────────────┤
│  VmBackend trait                             │
│  async fn start / stop / console_fds         │
├──────────┬───────────────────────────────────┤
│  macOS:  │  (future: Linux via libkrun)      │
│  Apple   │                                   │
│  Virt.   │                                   │
│  via     │                                   │
│  objc2   │                                   │
├──────────┴───────────────────────────────────┤
│  Asset Manager                               │
│  Download & cache Alpine kernel + initramfs  │
└──────────────────────────────────────────────┘
```

## Key Technical Decisions

### 1. Apple Virtualization bindings: `objc2-virtualization`

Use the `objc2-virtualization` crate (auto-generated from Xcode SDK, part of the objc2 ecosystem). It gives 1:1 access to every Virtualization.framework class with no Swift toolchain needed. All `unsafe` calls are confined to `vm/apple.rs` behind a safe `VmBackend` trait.

Rejected alternatives:
- `virtualization-rs` -- depends on unmaintained `objc` 0.2 crate
- `swift-bridge` -- unnecessary build complexity
- Manual C FFI to Swift -- most work, least benefit

### 2. Threading: dispatch queue + tokio, separate

`VZVirtualMachine` must be used from the queue it was created on. We create a dedicated serial dispatch queue (via `dispatch2`) for VM operations. Tokio runs separately for async I/O. They communicate through `tokio::sync::oneshot` channels and OS pipe fds.

### 3. Rootfs: Alpine Linux netboot artifacts (aarch64)

Download `vmlinuz-virt` + `initramfs-virt` from Alpine's CDN on first run. The `virt` kernel has all virtio drivers built-in. Cache at `~/.ezpez/cache/`. Stock initramfs will drop to a shell if no root device is found -- which is exactly what we want for the MVP.

If the stock initramfs doesn't cooperate, fallback: build a custom cpio with Alpine minirootfs + a 10-line `/init` script.

### 4. Console: virtio console + pipe fds

Create Unix pipes, attach to `VZVirtioConsoleDeviceSerialPortConfiguration` via `VZFileHandleSerialPortAttachment`. Kernel boots with `console=hvc0`. Terminal set to raw mode via `crossterm`, two async tasks relay stdin/stdout bytes.

## Module Structure

```
src/
  main.rs              -- tokio entrypoint, orchestration
  cli.rs               -- clap definitions
  error.rs             -- thiserror error type
  vm/
    mod.rs             -- VmBackend trait
    config.rs          -- VmConfig (cpus, memory, kernel path)
    apple.rs           -- Apple Virtualization implementation [cfg(macos)]
  assets/
    mod.rs             -- download, verify, cache logic
    alpine.rs          -- Alpine URLs, checksums, file layout
  terminal/
    mod.rs             -- raw mode guard, stdin/stdout relay
```

## Phased Plan

### Phase 0: Project Skeleton

Set up dependencies, module structure, clap CLI.

1. Update `Cargo.toml` with all dependencies (clap, tokio, objc2-virtualization, crossterm, reqwest, sha2, thiserror, directories, tracing)
2. Put `objc2-*`/`block2`/`dispatch2` behind `[target.'cfg(target_os = "macos")'.dependencies]`
3. Create all module files with stub implementations
4. Define `VmBackend` trait, `VmConfig` struct, CLI args
5. Wire `main.rs` with tokio + tracing

**Verify**: `cargo check` passes, `cargo run -- --help` prints usage.

### Phase 1: Asset Manager

Download and cache Alpine kernel + initramfs on first run.

1. Implement `assets/alpine.rs` -- URLs and SHA-256 checksums for Alpine 3.x aarch64 netboot
2. Implement `assets/mod.rs` -- `ensure_assets() -> Result<AssetPaths>` that checks cache, downloads if missing, verifies checksums
3. Show download progress to stderr

**Verify**: First run downloads ~19 MB to `~/.ezpez/cache/`, second run skips.

### Phase 2: VM Boot

Boot a Linux VM and see kernel messages on stdout.

1. Implement `vm/apple.rs`:
   - Create pipes for console I/O
   - Configure `VZLinuxBootLoader` with kernel path, `console=hvc0` command line, initramfs
   - Configure `VZVirtualMachineConfiguration` (CPU, memory, console, entropy device, balloon, platform)
   - Validate configuration
   - Create VM on a serial dispatch queue
   - Implement `start()` -- calls `startWithCompletionHandler`, bridges to oneshot channel
   - Implement `stop()` -- calls `stopWithCompletionHandler`
2. Wire into `main.rs`: ensure assets -> create backend -> start -> read console output to stdout
3. Handle entitlement issues if they arise (ad-hoc codesign)

**Verify**: `cargo run` shows Linux kernel boot messages in terminal.

### Phase 3: Interactive Shell

Full interactive terminal with raw mode and bidirectional I/O.

1. Implement `terminal/mod.rs`:
   - `TerminalGuard` -- enables raw mode on creation, disables on drop (+ panic hook)
   - Relay loop: two tokio tasks (stdin->guest, guest->stdout) using `AsyncFd` on pipe fds
   - `tokio::select!` to wait for either task or VM stop
2. Implement `VZVirtualMachineDelegate` to detect guest shutdown
3. Handle signals: Ctrl+C is forwarded as byte 0x03 in raw mode (automatic), SIGTERM triggers graceful stop

**Verify**: Interactive shell works. Type `ls /`, `uname -a`. Ctrl+C works. `poweroff`/`exit` cleanly exits and restores terminal.

### Phase 4: Polish

1. Startup message ("Booting VM...") before raw mode
2. `quiet loglevel=0` in kernel cmdline (suppress boot noise), `--verbose` to restore
3. `--kernel` and `--initramfs` override flags
4. Graceful error messages for common failures
5. Integration test: boot VM, send command via pipe, assert output, shut down

**Verify**: Clean end-to-end user experience. `cargo test` passes.

## Risks

| Risk | Mitigation |
|------|------------|
| `objc2-virtualization` lacks examples | API mirrors Apple's ObjC 1:1; reference `Code-Hex/vz` (Go) and `evansm7/vftool` |
| Stock Alpine initramfs doesn't drop to shell | Build custom initramfs (minirootfs + `/init` script) |
| macOS entitlement required | Ad-hoc codesign: `codesign --entitlements entitlements.plist -s - target/debug/ezpez` |
| dispatch queue + tokio threading complexity | Strictly separated; communicate only via oneshot channels and pipe fds |
