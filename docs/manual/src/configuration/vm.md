# VM options

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

The VM boundary is already the primary isolation layer, but `harden`
(enabled by default) adds a second belt inside the guest: namespace
restrictions and the `no-new-privileges` flag on the container process.
`no-new-privileges` means a setuid binary inside the sandbox can no longer
elevate to root — useful against local-privilege-escalation tricks an
agent might trip into, even though the blast radius is already confined
to the VM.

```toml
[vm]
harden = true   # default
```

Disable it only when a workload genuinely needs the broader kernel
capabilities it takes away — the most common case is running Docker
inside the VM, which needs to create its own namespaces and mounts.

## Nested KVM (Linux only)

On Linux hosts with KVM support, you can expose `/dev/kvm` into the
guest so VMs running *inside* the sandbox get hardware acceleration:

```toml
[vm]
kvm = true
```

This is what you need for, say, running `qemu-system-*` or another
hypervisor from inside the sandbox without falling back to software
emulation. It's only available on Linux and requires `/dev/kvm`
access on the host — Apple Virtualization on macOS doesn't expose
nested virt.

