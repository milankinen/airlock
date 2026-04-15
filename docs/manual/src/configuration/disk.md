# Disk and Cache

airlock creates a sparse ext4 disk image for each project sandbox. This disk
persists writes that happen outside of mounted host directories — things like
installed system packages, global tool caches, or any files the container
process creates on the root filesystem.

The disk is 10 GB by default (sparse, so it only uses actual disk space for
data written). You can change the size:

```toml
[disk]
size = "20 GB"
```

## Resizing

If you increase `disk.size` in the config, airlock grows the disk image on the
next start and the ext4 filesystem is automatically expanded inside the VM.
Existing data is preserved — this is a safe operation.

If you decrease `disk.size`, the disk image is deleted and recreated at the
new size. This means **all data on the disk is lost**, including installed
packages and any state not backed by named caches. There is no in-place
shrink — the only way to reduce the disk size is a full reset.

## Named caches

When you change the project's OCI image (e.g. upgrading from `ubuntu:22.04`
to `ubuntu:24.04`), the disk contents are reset to match the new image. This
is usually what you want — a clean slate — but some directories are worth
preserving across image changes.

Named caches solve this. Each cache entry lists one or more container paths
that should be backed by persistent storage that survives image changes:

```toml
[disk.cache.cargo]
paths = ["~/.cargo/registry"]

[disk.cache.node-modules]
paths = ["node_modules"]
```

Relative paths (like `node_modules`) are resolved relative to the project
directory inside the container. Paths starting with `~` are expanded to the
container user's home directory.

This is especially useful for package manager caches. Without named caches,
switching the base image would force a full re-download of all dependencies.
With them, `cargo build` or `npm install` picks up right where it left off.

A cache can be temporarily disabled without removing it from the config:

```toml
[disk.cache.cargo]
enabled = false
paths = ["~/.cargo/registry"]
```
