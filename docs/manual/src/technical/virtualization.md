# Virtualization

## macOS: Apple Virtualization framework

airlock uses the native `com.apple.Virtualization` framework via Rust
bindings (`objc2-virtualization`). The binary requires two
entitlements and must be ad-hoc codesigned after every build:

```
com.apple.security.virtualization
com.apple.security.hypervisor
```

## Linux: Cloud Hypervisor + KVM

airlock uses [Cloud Hypervisor](https://www.cloudhypervisor.org/) with
KVM acceleration. The `airlock` binary embeds the `cloud-hypervisor` and
[`virtiofsd`](https://gitlab.com/virtio-fs/virtiofsd) binaries and
extracts them on first run. KVM requires read and write access to
`/dev/kvm`. At startup, `airlock` opens the device to check this access.
If the device does not open, `airlock` shows the cause and stops.

## Kernel and initramfs

The build compiles the kernel and initramfs from source and embeds them
into the `airlock` binary via `include_bytes!`. Shipping them inside the binary
means there are no runtime downloads — one self-contained executable
starts a full Linux VM, which also makes offline use and reproducible
deployments straightforward.

- **Kernel**: Linux built from source with a minimal config (no
  EFI stub — VZLinuxBootLoader and Cloud Hypervisor both require a
  raw ARM64 `Image`). Built inside Docker.
- **Initramfs**: Alpine-based with the `airlockd` supervisor binary
  and a minimal init script. Built inside Docker as a gzipped cpio
  archive.
- airlock extracts both to `~/.cache/airlock/vm/` on first run. A
  checksum check re-extracts them if the binary is updated.

The [distroless build variant](../advanced/custom-kernel.md) omits
the embedded kernel and initramfs. The user provides them via
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
