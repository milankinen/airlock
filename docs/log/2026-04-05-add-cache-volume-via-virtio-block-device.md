# Add cache volume via VirtIO block device

VirtioFS mounts have significant overhead for metadata-heavy operations
(builds, package managers). A VirtIO block device with ext4 provides
native filesystem performance inside the VM for cache data.

### Config

```toml
[cache]
size = "20 GB"
mounts = ["~/.cache", "/cache"]
```

The `[cache]` section is fully optional — omitting it means no cache.
Removing it after use deletes the disk image. Size uses smart-config's
built-in `ByteSize` type (parses "20 GB", "512 MB", "4 KiB", etc.),
which also replaced the old `memory_mb: u64` with `memory: ByteSize`.

### Disk image lifecycle

- **Create**: sparse raw file via `File::set_len()` (no actual disk use)
- **Grow**: `set_len()` to new size + `resize2fs` in init expands fs
- **Shrink**: delete + recreate (ext4 reformatted on next boot)
- **Remove**: deleting `[cache]` from config removes `cache.img`

### Architecture

Host CLI creates a sparse raw disk image and attaches it as a VirtIO
block device via `VZDiskImageStorageDeviceAttachment` +
`VZVirtioBlockDeviceConfiguration`. The init script formats ext4 on
first use and mounts at `/mnt/cache`. Cache mount subdirs use the
container target path (without leading `/`) so reordering mounts in
config doesn't mix up data. Subdirs are created by the supervisor
via a new `cacheDirs` RPC field, keeping init simple.

### Files changed

- `cli/src/config.rs` — `Cache` struct, `ByteSize` for memory+cache
- `cli/src/oci/cache.rs` — new: disk image management + mount resolution
- `cli/src/oci.rs` — `MountType::Cache`, `Bundle.cache_image`, `cache_dirs()`
- `cli/src/vm/config.rs` — `cache_disk: Option<PathBuf>`
- `cli/src/vm/apple.rs` — VirtIO block device attachment
- `cli/src/vm.rs` — skip VirtioFS for cache, pass cache_disk, display
- `cli/Cargo.toml` — objc2-virtualization block device features
- `protocol/schema/supervisor.capnp` — `cacheDirs` in start RPC
- `cli/src/rpc/supervisor.rs` — send cacheDirs
- `sandbox/supervisor/src/rpc.rs` — receive cacheDirs
- `sandbox/supervisor/src/main.rs` — create cache subdirs
- `sandbox/rootfs/init` — ext4 format, mount, resize2fs
- `sandbox/rootfs/build.sh` — e2fsprogs package
