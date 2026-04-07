# Rework VM mount architecture with shared rootfs and overlay

Major rework of how the container filesystem is assembled. Previously the
image rootfs was CoW-copied per project and mounts were split between OCI
config and supervisor. Now the image rootfs is shared read-only via VirtioFS,
and all mounting happens in the supervisor before crun starts.

### Architecture

VirtioFS shares: `base` (shared image rootfs, read-only), `overlay`
(project-specific config, file mounts, OCI config), `project` + `mount_N`
(user dir mounts). Plus a VirtIO block device (ext4, always present, 10GB
default) for the overlayfs upper layer and cache dirs.

Host-side overlay dir layout:
```
<project>/.ez/overlay/
  config.json       OCI runtime config
  mounts.json       mount mappings for the supervisor
  rootfs/           empty — overlayfs mount point
  files_rw/         file mounts with target dir structure
  files_ro/         read-only file mounts
```

Disk layout:
```
/mnt/disk/
  overlay/
    .image_id       tracks base image for reset
    rootfs/         overlayfs upper layer
    work/           overlayfs work dir
  cache/
    <target paths>  cache-backed directories
```

The supervisor reads `mounts.json` and assembles the rootfs:
1. overlayfs: lowerdir=base, upperdir=disk/overlay/rootfs
2. symlink file mounts into /.ez/files_rw (or files_ro)
3. bind-mount dir mounts (project dir, user mounts)
4. bind-mount cache dirs (last — overrides dir mounts)

### VirtioFS file mount workaround

VirtioFS doesn't support file-level bind mounts — stat/ls works but
data reads fail with EACCES. Directory bind mounts work fine. The
workaround: OCI config binds the files_rw/files_ro directories to
`/.ez/` inside the container, and the supervisor creates symlinks from
target paths (e.g. `/root/.claude.json`) into `/.ez/files_rw/...`.
Reads and writes go through the symlink → directory bind mount →
VirtioFS → host file.

### Key decisions

- **Overlay upper on ext4 disk, not VirtioFS**: overlayfs requires a
  local filesystem for upper/work — VirtioFS is FUSE-based.
- **Always-present disk** (default 10GB sparse): serves as both overlay
  upper and cache. No more optional cache-only disk.
- **Image ID tracking**: stored on disk at `overlay/.image_id`. Supervisor
  resets the overlay dir when the image changes. Cache dirs survive.
- **Shutdown RPC**: host calls `supervisor.shutdown()` after process exits,
  supervisor calls `sync()` to flush ext4 writes before VM is killed.
- **No bundle dir**: config.json and mounts.json live in the overlay dir.
  Removed the separate bundle VirtioFS share.

### Other fixes in this batch

- ext4 support added to kernel configs (CONFIG_EXT4_FS=y)
- e2fsprogs-extra added to rootfs (provides resize2fs)
- System binaries use absolute paths (/sbin/mkfs.ext4, /usr/sbin/iptables)
- mise build tasks: kernel/libkrun as dependencies of build:dev with OS gates
