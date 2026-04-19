# Virtualization

## macOS: Apple Virtualization framework

Uses the native `com.apple.Virtualization` framework via Rust
bindings (`objc2-virtualization`). The binary requires two
entitlements and must be ad-hoc codesigned after every build:

```
com.apple.security.virtualization
com.apple.security.hypervisor
```

## Linux: Cloud Hypervisor + KVM

Uses [Cloud Hypervisor](https://www.cloudhypervisor.org/) with KVM
acceleration. The `cloud-hypervisor` and
[`virtiofsd`](https://gitlab.com/virtio-fs/virtiofsd) binaries are
embedded in the `airlock` binary and extracted on first run.
Requires `/dev/kvm` access; `airlock` checks and reports permission
issues at startup.

## Kernel and initramfs

Kernel and initramfs are built from source and embedded into the
`airlock` binary via `include_bytes!`. Shipping them inside the binary
means there are no runtime downloads — one self-contained executable
boots a full Linux VM, which also makes offline use and reproducible
deployments straightforward.

- **Kernel**: Linux built from source with a minimal config (no
  EFI stub — VZLinuxBootLoader and Cloud Hypervisor both require a
  raw ARM64 `Image`). Built inside Docker.
- **Initramfs**: Alpine-based with the `airlockd` supervisor binary
  and a minimal init script. Built inside Docker as a gzipped cpio
  archive.
- Both are extracted to `~/.cache/airlock/vm/` on first run. A
  checksum check re-extracts them if the binary is updated.

The [distroless build variant](../advanced/custom-kernel.md) omits
the embedded kernel and initramfs; the user provides them via
`vm.kernel` and `vm.initramfs` in the config.

## Virtio devices

| Device          | Purpose                                               |
|-----------------|-------------------------------------------------------|
| Serial console  | Kernel debug output                                   |
| Entropy         | `/dev/urandom` in guest                               |
| Memory balloon  | Future: reclaim unused guest memory                   |
| vsock           | Host ↔ guest RPC (port 1024)                          |
| [VirtioFS](https://virtio-fs.gitlab.io/) | Shared filesystems (image layers, dir/file mounts) |
| Block (ext4)    | Per-project persistent disk                           |
