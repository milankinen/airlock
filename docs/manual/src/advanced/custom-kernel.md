# Custom Kernel

By default, the standard (bundled) build of airlock ships with a Linux kernel
and initramfs compiled into the binary. This is the easiest way to get
started — no extra files to manage, and the kernel is known to work with the
`airlockd` guest supervisor.

For situations where the bundled kernel doesn't fit — custom drivers, a
different kernel version, or a stripped-down build — airlock supports
pointing to external kernel and initramfs files.

## Configuration

Set the `kernel` and `initramfs` fields in the `[vm]` section:

```toml
[vm]
kernel = "/path/to/vmlinux"
initramfs = "/path/to/initramfs.cpio.gz"
```

Both paths support `~` expansion and are resolved relative to the project
directory. When these are set, airlock uses them instead of the bundled files.

The kernel must be compatible with airlock's guest supervisor (`airlockd`).
The repository's
[app/vm-kernel](https://github.com/milankinen/airlock/tree/main/app/vm-kernel)
directory contains the kernel configs used for official builds — these are
a good starting point if you want to compile your own.

## Distroless builds

airlock is published in two variants:

- **Bundled** — includes the Linux kernel and initramfs in the binary. This
  is the default and recommended variant for most users.
- **Distroless** — a smaller binary with no bundled kernel or initramfs.
  The `vm.kernel` and `vm.initramfs` config fields become required.

The distroless variant is useful when you already maintain your own kernel
builds, want to minimize binary size, or need a kernel with specific patches
or driver support. It's also the right choice for packaging airlock into
environments where bundling a kernel would conflict with the host's kernel
management.

Install the distroless variant with:

```bash
curl -fsSL https://raw.githubusercontent.com/milankinen/airlock/main/install.sh | sh -s -- --distroless
```

## Building a kernel

The official kernel build script lives at `app/vm-kernel/build.sh` in the
repository. It downloads the configured Linux version, applies the airlock
kernel config, and produces a kernel image and initramfs. Both x86_64 and
ARM64 architectures are supported.

If you're building a custom kernel from scratch, the key requirements are:

- VirtIO drivers (network, block, filesystem, vsock)
- 9p and VirtioFS filesystem support
- ext4 filesystem support
- Overlayfs support
- The init system must be compatible with the `airlockd` supervisor binary
  that gets packed into the initramfs
