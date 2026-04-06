# Rework VM mount architecture: shared image rootfs + project overlay

## Context

Currently the image rootfs is CoW-copied to each project's bundle dir, and
mounts are split between the OCI config (dir mounts, cache) and the supervisor
(file overlay). This causes ordering issues where cache and VirtioFS mounts
interfere with each other.

New design: the image rootfs is shared read-only via VirtioFS, and a single
project overlay dir handles all per-project state. The supervisor does ALL
mounting before crun starts. OCI config only has system mounts.

## VirtioFS shares (new)

| Tag | Host path | Mode | Contents |
|-----|-----------|------|----------|
| `base` | `~/.ezpez/images/<digest>/rootfs` | read-only | Image rootfs |
| `overlay` | `<project>/.ez/overlay/` | read-write | `upper/`, `work/`, `files_rw/`, `files_ro/` |
| `project` | `<project cwd>` | read-write | Project source directory |
| `mount_N` | user-defined | per config | User directory mounts |
| `bundle` | `<project>/.ez/bundle/` | read-write | OCI config.json only (no rootfs!) |
| cache disk | `<project>/.ez/cache.img` | block device | ext4, optional |

## Supervisor mount assembly (in order)

```
1. Mount all VirtioFS shares at /mnt/<tag>
2. Mount cache disk at /mnt/cache (if present)
3. Ensure cache subdirs exist on disk

4. overlayfs on /mnt/bundle/rootfs:
     lowerdir = /mnt/overlay/files_rw : /mnt/overlay/files_ro : /mnt/base
     upperdir = /mnt/overlay/upper  (or /mnt/cache/.overlay_upper if cache)
     workdir  = /mnt/overlay/work   (or /mnt/cache/.overlay_work if cache)

5. Bind-mount rw files from /mnt/overlay/files_rw onto rootfs
6. Bind-mount dir mounts:
     /mnt/project → rootfs/<project cwd>
     /mnt/mount_0 → rootfs/<target_0>
     ...
7. Bind-mount cache dirs:
     /mnt/cache/<rel> → rootfs/<target>
8. DNS (write resolv.conf to rootfs)
```

## Mount info: mounts.json

Instead of extending the RPC schema, write a `mounts.json` to the bundle dir.
The supervisor reads `/mnt/bundle/mounts.json` after VirtioFS is mounted.

```json
{
  "dirs": [
    {"tag": "project", "target": "/Users/mla/dev/ezpez", "read_only": false},
    {"tag": "mount_0", "target": "/root/.claude", "read_only": false}
  ],
  "cache": ["/Users/mla/dev/ezpez/target"]
}
```

## Changes by file

### Phase 1: Host side

**`cli/src/oci.rs`**
- Remove rootfs CoW-copy to bundle. Bundle dir only has `config.json` and `mounts.json`.
- `build_bundle()`: write `mounts.json` with dir mounts + cache targets.
- `install_ca_cert()`: write CA certs to `overlay/upper/` instead of `bundle/rootfs/`.
  (They'll appear in the overlay's upper layer, visible in the assembled rootfs.)
- `lookup_home_dir()`: read from image_dir rootfs (not bundle rootfs).
- `Bundle` struct: remove `cache_targets`, add `image_rootfs: PathBuf`.

**`cli/src/oci/config.rs`**
- Remove ALL bind mounts and cache mounts from OCI config.
- Remove `cache_targets` parameter.
- `rootfs` path stays as `"rootfs"` — crun sees the assembled overlay.

**`cli/src/vm.rs`**
- Add `base` share (image rootfs, read-only).
- Add `overlay` share (project overlay dir, read-write).
- `link_file()`: put files in `overlay_dir/files_rw/` or `overlay_dir/files_ro/`
  instead of a separate `files/` dir.
- Remove separate `files_rw`/`files_ro` VirtioFS shares.
- Create overlay subdirs: `upper/`, `work/`, `files_rw/`, `files_ro/`.
- Ensure `bundle/rootfs/` dir exists (empty — overlay fills it).

**`cli/src/oci/cache.rs`**
- No changes to disk image creation.
- `cache_targets` still returned for `mounts.json`.

**`cli/src/main.rs`**
- Remove `cache_dirs` and `has_cache_disk` from `supervisor.start()` args.
- Remove `share_tags` — supervisor reads shares from mounts.json.

**`cli/src/rpc/supervisor.rs`**
- Remove `cache_dirs`, `shares`, `has_cache_disk` from RPC call.
- Keep: `host_ports`, `epoch`, `tls_passthrough` (needed for networking setup).

### Phase 2: RPC / Protocol

**`protocol/schema/supervisor.capnp`**
- Remove `cacheDirs`, `shares`, `hasCacheDisk` from `start()`.
- Keep: `hostPorts`, `epoch`, `tlsPassthrough`.

### Phase 3: Supervisor

**`sandbox/supervisor/src/init.rs`**
- `InitConfig`: remove `shares`, `has_cache_disk`, `cache_dirs`.
- Keep: `host_ports`, `epoch`.

**`sandbox/supervisor/src/init/linux.rs`**
- `setup()`: new flow:
  1. `set_clock()`
  2. `mount_virtiofs_shares()` — mount well-known tags: `base`, `overlay`, `bundle`,
     then read `/mnt/bundle/mounts.json` for additional tags.
  3. `setup_networking()`
  4. `setup_cache_disk()` — detect `/dev/vda`, format/mount at `/mnt/cache`.
  5. `assemble_rootfs()` — overlayfs + file bind mounts + dir bind mounts + cache bind mounts.
  6. `setup_dns()`

- `assemble_rootfs()`: new function that replaces `overlay_file_mounts()`:
  1. overlayfs with base + overlay → rootfs
  2. bind-mount rw files
  3. bind-mount dirs from mounts.json
  4. bind-mount cache dirs from mounts.json

**`sandbox/supervisor/src/rpc.rs`**
- Simplify `InitConfig` construction (fewer RPC fields).

### Phase 4: Cleanup

- Remove `cow_copy` usage for rootfs (may keep function for other uses).
- Remove `MountType::File` key/vm_path logic (files handled by overlay dir).
- Update `cli/src/oci.rs` `ResolvedMount` — simplify since OCI config doesn't use vm_path.

## Verification

1. `mise run lint` passes
2. `mise run build:supervisor` passes
3. `cargo test -p ezpez-cli` — all 68 tests pass
4. Manual test: `mise run ez` — VM boots, project dir writable, cache dir persists,
   file mounts work, `apt install` persists across restarts (in overlay upper)
5. Delete `.ez/overlay/upper/` — resets project to clean image state
