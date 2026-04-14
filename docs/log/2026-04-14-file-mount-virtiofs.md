# Fix file mount sync: per-file VirtioFS share instead of hard-link

## Problem

File mounts (single-file entries in `[mounts]`) were implemented by hard-linking
the source file into `overlay/files/rw/{target_path}/` on the host, then exposing
that overlay directory as a VirtioFS share in the guest. Symlinks inside the
container rootfs pointed into that shared overlay directory.

This broke on cross-filesystem scenarios (e.g. when `airlock up` is run inside a
VM where the project is on a VirtioFS mount and the cache is on ext4):
`std::fs::hard_link()` returns `EXDEV`, the code fell back to `std::fs::copy()`,
and writes inside the nested VM updated only the copy in the cache — never the
original file.

## Fix

Each file mount now gets its own dedicated VirtioFS share whose host path is the
**source file's parent directory** (not a copy in the overlay). The share tag is
derived from the config key name (e.g. `file-claude-json` for `[mounts.claude-json]`).

Inside the container, `setup_container_mounts()` mounts that VirtioFS share
directly at `/airlock/.files/{tag}/` (inside the overlayfs rootfs), then creates
a symlink from the target path to `/airlock/.files/{tag}/{source_filename}`.

Write path: container target path → symlink → `/airlock/.files/{tag}/{filename}` →
VirtioFS share → source file on host. No hard-linking, no copy, no intermediate
cache directory involved.

### Changes

- `supervisor.capnp`: added `tag` and `filename` fields to `FileMount`
- `oci.rs`: `MountType::File` now carries `tag` (config key prefixed with `file-`)
  and `filename` (source basename); `resolve_mounts` accepts `(name, Mount)` pairs
- `vm.rs`: removed `link_file()` and the `overlay/files/{rw,ro}` directory
  overhead; each file mount adds a `VmShare` for its source parent directory
- `rpc/supervisor.rs` (host): sends `tag` and `filename` in file mount RPC
- `init.rs` (guest): `FileMountConfig` gains `tag` and `filename` fields
- `rpc.rs` (guest): reads new fields from RPC params
- `init/linux.rs` (guest): mounts each file's VirtioFS share directly inside the
  container rootfs, removed the old `files/rw`/`files/ro` bind mount approach;
  added `mount_virtiofs_at()` helper for mounting at arbitrary paths
