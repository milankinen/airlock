# MVP — Boot a Linux VM with interactive shell

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
