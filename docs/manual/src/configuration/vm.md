# VM Options

The `[vm]` section controls the virtual machine image and resource allocation.

## Image

The `image` field specifies which OCI image to use as the container root
filesystem. By default, airlock uses `alpine:latest`:

```toml
[vm]
image = "ubuntu:24.04"
```

For more control — for example when pulling from a private or local
registry — use the object form:

```toml
[vm.image]
name = "registry.company.com/base-image:latest"
resolution = "registry"
```

The `resolution` field controls where airlock looks for the image:

- `auto` (default) — try the local Docker daemon first, fall back to the OCI
  registry. This is convenient if you already have the image locally.
- `docker` — only use the local Docker daemon. Fails if the image isn't found.
- `registry` — always pull from the OCI registry, ignore Docker entirely.
  This is the right choice when Docker isn't installed.

For development registries served over plain HTTP, set `insecure = true`:

```toml
[vm.image]
name = "localhost:5005/dev-image:latest"
resolution = "registry"
insecure = true
```

## Resources

By default, airlock allocates all available host CPUs and half the system RAM
to the VM. You can override these:

```toml
[vm]
cpus = 4
memory = "4 GB"
```

Memory accepts human-readable sizes like `"512 MB"`, `"4 GB"`, or `"2G"`.
The minimum is 512 MB, and the maximum is the total system RAM.

## Security hardening

The `harden` option (enabled by default) applies additional isolation inside
the VM: namespace restrictions and the `no-new-privileges` flag. This
prevents processes in the container from escalating their permissions:

```toml
[vm]
harden = true   # default
```

You might need to disable hardening if you're running Docker inside the VM or
doing other tasks that require broader kernel capabilities.

## Nested KVM (Linux only)

On Linux hosts with KVM support, you can enable nested virtualisation inside
the VM:

```toml
[vm]
kvm = true
```

This is only available on Linux and requires `/dev/kvm` access on the host.

## Custom kernel and initramfs

If you're using the **distroless** build of airlock (which doesn't bundle a
kernel), or you need a custom kernel for other reasons, you can point to
external files:

```toml
[vm]
kernel = "/path/to/vmlinux"
initramfs = "/path/to/initramfs.cpio.gz"
```

For details on the distroless build variant, kernel requirements, and
building your own kernel, see [Custom Kernel](../advanced/custom-kernel.md).
