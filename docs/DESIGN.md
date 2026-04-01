# Design

## Overview

ezpez runs untrusted code inside a lightweight Linux VM on macOS.
A single `ez` binary boots a VM, pulls an OCI container image, and
gives the user an interactive shell inside the container. The VM
provides hardware-level isolation; the container provides a familiar
image-based environment.

```
┌─────────────────────────────────────────────────────────┐
│ Host (macOS)                                            │
│                                                         │
│  ez (CLI)                                               │
│  ├─ Pull OCI image from registry                        │
│  ├─ Prepare container bundle (rootfs + config.json)     │
│  ├─ Boot Linux VM (Apple Virtualization framework)      │
│  │   ├─ Kernel + initramfs embedded in binary           │
│  │   ├─ VirtioFS shares container bundle into VM        │
│  │   └─ vsock connects CLI ↔ supervisor                 │
│  ├─ RPC: exec container via supervisor                  │
│  └─ Relay terminal I/O (stdin/stdout/stderr)            │
│                                                         │
│  ┌───────────────────────────────────────────────────┐  │
│  │ VM (Linux, ARM64)                                 │  │
│  │                                                   │  │
│  │  init                                             │  │
│  │  ├─ Mount filesystems (proc, sys, dev, cgroup2)   │  │
│  │  ├─ Mount VirtioFS share at /mnt/bundle           │  │
│  │  └─ Launch supervisor                             │  │
│  │                                                   │  │
│  │  supervisor (Rust, static musl binary)            │  │
│  │  ├─ Listen on vsock port 1024                     │  │
│  │  ├─ Accept RPC connection from CLI                │  │
│  │  └─ On exec: spawn crun with PTY                  │  │
│  │                                                   │  │
│  │  crun (OCI runtime)                               │  │
│  │  └─ Run container from /mnt/bundle                │  │
│  │     ├─ PID, UTS, mount namespace isolation        │  │
│  │     └─ Shell or image entrypoint on PTY           │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

## Virtualization

- **macOS**: Apple Virtualization framework via Rust bindings
  (`objc2-virtualization`). Future: Linux support via `libkrun`.
- **Kernel**: Linux 6.18, built from source with a minimal config
  (PUI PUI Linux defconfig). No EFI stub — VZLinuxBootLoader
  requires a raw ARM64 Image.
- **Initramfs**: Alpine 3.23 with crun, supervisor binary, and a
  minimal init script. Built via Docker, packed as gzipped cpio.
- **Single binary**: Kernel and initramfs are embedded in the `ez`
  binary via `include_bytes!` and extracted to cache on first run.

### Virtio devices

| Device | Purpose |
|--------|---------|
| Serial console | Debug output (kernel messages) |
| Entropy | `/dev/urandom` in guest |
| Memory balloon | Future: reclaim unused guest memory |
| vsock | Host ↔ guest RPC communication |
| VirtioFS | Share container bundle from host into guest |

### Entitlements

The binary requires macOS entitlements for virtualization and
hypervisor access. It must be ad-hoc codesigned after every build:

```
com.apple.security.virtualization
com.apple.security.hypervisor
```

## Communication: vsock + Cap'n Proto RPC

The CLI and supervisor communicate over a single vsock connection
using Cap'n Proto RPC (twoparty transport).

- **Guest listens, host connects** — the supervisor listens on vsock
  port 1024. The CLI retries connection after VM boot until the
  supervisor is ready. This avoids implementing ObjC delegate
  protocols on the host side.

## Container execution

### OCI image pulling

Images are pulled from OCI registries (Docker Hub, etc.) on the
host side. The CLI resolves the image reference to a manifest and
digest, downloads layer blobs, and extracts them into a merged
rootfs directory.

Platform is fixed to `linux/arm64` (matching the VM architecture).

### Caching

```
~/.ezpez/
  kernel/
    Image                    # extracted from embedded binary (once)
    initramfs.gz
  images/
    <digest>/                # one per unique image manifest
      rootfs/                # all layers merged
      image_config.json
      layer_*.tar.gz         # raw layer blobs
  projects/
    <hash>/                  # one per working directory
      bundle/
        config.json          # OCI runtime spec
        rootfs/              # APFS copy-on-write clone of image rootfs
      image_digest           # tracks which image this project uses
```

- **Image cache** is keyed by manifest digest. Shared across all
  projects using the same image.
- **Project cache** is keyed by `sha256(canonical_cwd)`. Each working
  directory gets its own persistent sandbox state. The rootfs is an
  APFS copy-on-write clone of the cached image — instant creation,
  zero disk cost until files are modified.
- **Image change detection**: the stored `image_digest` file is
  compared against the resolved digest on each run. If the image
  changed, the project bundle is recreated.

### Container runtime

Containers run via **crun**, a lightweight OCI runtime (512KB). It
receives an OCI runtime bundle (config.json + rootfs) and creates
isolated namespaces for the container process.

- **`--no-pivot`** flag is required because VirtioFS doesn't support
  the `pivot_root` syscall. Uses `chroot` instead, which is fine
  since the VM is the security boundary.
- **PTY allocation**: crun allocates a pseudo-terminal for the
  container process, giving proper echo, line editing, and job
  control.
- **Namespaces**: PID, UTS, mount. Network and IPC namespaces
  available but not yet used.
- **config.json** is derived from the OCI image config: process
  args (CMD + ENTRYPOINT), environment variables, working directory,
  and user are read from the image manifest.

## Build pipeline

```
mise run build
  ├─ build:kernel      Docker: compile Linux 6.18 from source
  ├─ build:supervisor  Docker: cross-compile Rust (musl, ARM64)
  ├─ build:rootfs      Docker: Alpine + crun + supervisor → cpio
  └─ build (cargo)     Compile CLI, embed kernel + rootfs, codesign
```

All build tasks use Docker containers. The supervisor builder image
caches the Rust toolchain, Cap'n Proto compiler, and musl cross-
compilation target. Mise tasks have `sources`/`outputs` for
incremental builds (2ms no-op when nothing changed).

## Future work

- **`ez.toml` configuration** — sandbox settings in version control
- **Network filtering** — HTTP(S) proxy in VM, policy enforcement
  and secrets injection on the host side
- **File/directory mounting** — selective host directory sharing via
  additional VirtioFS mounts
- **Environment variable injection** — pass host env vars or secrets
  into the container
- **Memory reclaim** — use the virtio balloon device to return
  unused guest memory to the host
- **Linux host support** — `libkrun` backend as alternative to
  Apple Virtualization framework
- **Multi-session** — attach to an existing running VM instead of
  booting a new one each time
