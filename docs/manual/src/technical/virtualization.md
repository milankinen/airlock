# Virtualization

The `vmm` setting in `~/.airlock/settings.toml` selects the VM backend:

| Value              | Backend                         | Default on |
|--------------------|---------------------------------|------------|
| `apple-virt`       | Apple Virtualization framework  | macOS      |
| `cloud-hypervisor` | Cloud Hypervisor and KVM        | Linux      |
| `libkrun`          | libkrun                         | -          |

If the host cannot run the selected backend, the sandbox start stops
with an error.

## macOS: Apple Virtualization framework

airlock uses the native `com.apple.Virtualization` framework via Rust
bindings (`objc2-virtualization`). The binary requires two
entitlements and must be ad-hoc codesigned after every build:

```
com.apple.security.virtualization
com.apple.security.hypervisor
```

On supported Macs, you can enable nested virtualization to run VMs inside the sandbox.
See [Nested KVM](../configuration/vm.md#nested-kvm) for requirements and configuration.

## Linux: Cloud Hypervisor + KVM

airlock uses [Cloud Hypervisor](https://www.cloudhypervisor.org/) with
KVM acceleration. The `airlock` binary embeds the `cloud-hypervisor` and
[`virtiofsd`](https://gitlab.com/virtio-fs/virtiofsd) binaries and
extracts them on first run. KVM requires read and write access to
`/dev/kvm`. At startup, `airlock` opens the device to check this access.
If the device does not open, `airlock` shows the cause and stops.

airlock uses the [Landlock](https://docs.kernel.org/userspace-api/landlock.html)
security module of the host kernel to limit the file access of
`virtiofsd` and `cloud-hypervisor`. Each `virtiofsd` process can reach
only its shared directory, its socket directory and the system
libraries. When `airlock` runs as root, `virtiofsd` can also read
`/proc`. `cloud-hypervisor` can reach only the files of its VM
configuration. Thus a bug in one of these programs does not give the
guest access to the other files of the host user. Landlock needs Linux
6.2 or later. On an older kernel, or when Landlock is off, the programs
run without these limits, and airlock writes a warning to its log.

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
- airlock extracts both to `vm/` in the airlock data directory on first run. A
  checksum check re-extracts them if the binary is updated. Each VM
  backend extracts its own files in the same way, with a checksum of
  its own.

The [distroless build variant](../advanced/custom-kernel.md) omits
the embedded kernel and initramfs. The user provides them via
`vm.kernel` and `vm.initramfs` in the config.

## Virtio devices

| Device          | Purpose                                               |
|-----------------|-------------------------------------------------------|
| Serial console  | Kernel debug output                                   |
| Entropy         | `/dev/urandom` in guest                               |
| Memory balloon  | Gives free guest memory back to the host (Cloud Hypervisor, libkrun) |
| vsock           | Host ↔ guest RPC (ports 1024 and 1025)                |
| [VirtioFS](https://virtio-fs.gitlab.io/) | Shared filesystems (image layers, dir/file mounts) |
| Block (ext4)    | Per-project persistent disk                           |
