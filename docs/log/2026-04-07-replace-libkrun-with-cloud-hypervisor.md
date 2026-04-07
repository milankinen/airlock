# Replace libkrun with Cloud Hypervisor + virtiofsd

Replaced the libkrun VM backend with Cloud Hypervisor (CH) as a subprocess
plus external virtiofsd processes for VirtioFS shares.

## Why

libkrun had several fundamental issues:
- VirtioFS didn't support `trusted.*` xattrs needed for kernel overlayfs
  copy-up, forcing a fuse-overlayfs workaround or full base image copy
- TSI (Transparent Socket Impersonation) hijacked AF_INET sockets,
  breaking local networking (DNS, TCP proxy)
- Implicit console stole stdin/stdout
- Required building libkrun.so + libkrunfw.so from source and dlopen

## Architecture

Cloud Hypervisor + virtiofsd is the standard production approach (Kata
Containers). Both are static binaries — no dynamic linking.

```
Host: ez CLI
 ├── virtiofsd processes (one per VirtioFS share, with --xattr)
 ├── cloud-hypervisor subprocess (kernel, initramfs, vsock, disk)
 └── vsock connect via CONNECT protocol
```

virtiofsd supports `--xattr` for trusted xattr passthrough, enabling
kernel overlayfs without fuse-overlayfs. UID/GID mapping via
`--translate-uid/gid` maps guest root to host user.

## Key details

- **Kernel**: uses CH's `ch_defconfig` as base (from cloud-hypervisor/linux
  repo) with netfilter additions. Requires ACPI, PCI ECAM, X86_PAT, MTRR.
  Output: bzImage (not vmlinux — CH needs PVH or bzImage).
- **Block device**: `--disk path=...,image_type=raw` — explicit raw type
  prevents CH from blocking sector-zero writes (autodetection guard).
  mkfs.ext4 with `-E nodiscard` since CH virtio-blk rejects DISCARD.
- **Vsock**: connect to base socket (not `_<port>` variant), send
  `CONNECT <port>\n`, read `OK` response.
- **Multiple VirtioFS**: each share gets its own virtiofsd process with
  socket, CH `--fs` accepts multiple values after single flag.
- **Error propagation**: init failures now spawn a dummy exit-100 process
  so the RPC response can deliver the error message to the host.

## Removed

- `cli/src/vm/krun.rs` — libkrun dlopen backend
- `sandbox/libkrun/` — build scripts and netfilter config
- `mise/tasks/build/libkrun` — libkrun build task
- KVM access check in main.rs (CH handles its own errors)
