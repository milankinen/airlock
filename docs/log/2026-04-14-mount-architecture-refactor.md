# Mount architecture refactor: static shares + consolidated file share

## Problem

The previous file-mount implementation (per-file VirtioFS share) had two issues:

1. **Cross-filesystem hard-link failure**: When running inside a nested VM (project on
   VirtioFS, cache on ext4), `hard_link()` failed with `EXDEV`. The fallback `copy()` broke
   write-sync — container writes updated only the cache copy, not the original host file.

2. **One VirtioFS device per file**: Each file mount registered its own virtio device. With
   many file mounts this wastes virtio device slots and complicates the share list.

The wider architecture also mixed concerns: the `overlay` VirtioFS share exposed the entire
overlay directory to the guest (including the overlayfs upper layer), and `ca` certs were
bundled inside it rather than as an isolated read-only share.

## Design

### VirtioFS shares

Three static shares (always present):

| Tag | Host path | Mode |
|-----|-----------|------|
| `base` | image rootfs | ro |
| `project` | project host_cwd | rw |
| `ca` | `overlay/ca/` | ro (stronger isolation) |

User dir mounts are sorted by config key and tagged `dir_0`, `dir_1`, etc.

File mounts are consolidated into **two** shares:

| Tag | Host path |
|-----|-----------|
| `files/rw` | `overlay/files/rw/` (if any rw file mounts) |
| `files/ro` | `overlay/files/ro/` (if any ro file mounts) |

### File mount write path

For each file mount with config key `{key}`:

1. **Host** hard-links `source_file` → `overlay/files/rw/{key}` (copy fallback on EXDEV)
2. **VM boot**: `files/rw` VirtioFS share mounts at `/mnt/files/rw/`
3. **Overlayfs upper**: supervisor creates symlink `upper/{target_rel_path}` →
   `/airlock/.files/rw/{key}` before mounting overlayfs
4. **Container setup**: `/mnt/overlay/rootfs/airlock/.files/rw/` bind-mounted from
   `/mnt/files/rw/`

Write path inside container:
```
~/.claude.json → symlink in upper layer → /airlock/.files/rw/claude-json
               → bind mount → /mnt/files/rw/claude-json
               → VirtioFS → overlay/files/rw/claude-json (hard link) → host file
```

### Overlayfs

- `lowerdir=/mnt/ca:/mnt/base` — ca as highest-priority read-only layer
- `upperdir=/mnt/disk/overlay/upper` — renamed from `overlay/rootfs`
- `workdir=/mnt/disk/overlay/work`
- mount point: `/mnt/overlay/rootfs` (created as a local dir, no longer a VirtioFS share)

The `overlay` VirtioFS share is removed entirely.

### Filelinks

`/mnt/disk/filelinks/{key}` → `/airlock/.files/{rw|ro}/{key}` — persistent symlinks
that survive overlay resets. Stale entries (keys removed from config) are pruned on boot.

## Changes

- `supervisor.capnp`: `FileMount.tag` renamed to `key`, `filename` field removed
- `oci.rs`: `MountType::File { tag, filename }` → `{ mount_key }`;
  dir mounts tagged `dir_{N}` (sorted by config key); user mounts sorted before indexing
- `vm.rs`: removed `overlay` share; added `ca` share; hard-link file mounts into
  `overlay/files/{rw|ro}/`; conditional `files/rw` / `files/ro` shares
- `rpc/supervisor.rs`: sends `key` for file mounts
- `airlockd/init.rs`: `FileMountConfig` loses `tag`/`filename`, gains `mount_key`
- `airlockd/rpc.rs`: reads `key` field
- `airlockd/init/linux.rs`:
  - `setup()`: mounts `base`, `ca`, dir mounts, `files/rw`/`files/ro` conditionally;
    creates `/mnt/overlay/rootfs` as local dir
  - `assemble_rootfs()`: writes file symlinks into upper layer before overlayfs mount;
    `overlay/upper` (renamed from `overlay/rootfs`); lowerdir uses `/mnt/ca`
  - `setup_container_mounts()`: bind-mounts VirtioFS shares into `/airlock/.files/{rw|ro}/`
