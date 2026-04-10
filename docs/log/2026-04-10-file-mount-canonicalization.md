# File mount canonicalization

Two bugs in the file mount pipeline prevented relative paths from working as users expect
(`source = "mise.toml"`, `target = "mise.toml"` should resolve to the project directory's file).

## Bug 1 — Relative target paths resolved against container root

`resolve_mounts` called `expand_tilde` on the target but, unlike the source path, did not
resolve relative targets against any base directory. A bare `"mise.toml"` target became a
path with no leading `/`, which `assemble_rootfs` turned into a symlink at the container root
(`/mise.toml`) rather than at `<guest_cwd>/mise.toml`.

**Fix:** added `guest_cwd: &Path` to `resolve_mounts` and joined relative targets with it,
mirroring how source paths are joined with `cwd`.

## Bug 2 — File mount symlinks shadowed by dir bind mounts

`assemble_rootfs` wrote symlinks into the overlayfs upper layer for file mounts, then applied
dir bind mounts (including the project dir at `guest_cwd`) on top. Any file-mount symlink whose
target fell under a dir-mounted path was hidden: the bind mount for that directory covered the
entire tree, making the symlink unreachable.

**Fix:** removed the symlink approach entirely. Instead, per-file bind mounts are now applied in
`setup_container_mounts`, which runs _after_ `assemble_rootfs` has finished all dir bind mounts.
Each file is bind-mounted directly from its VirtioFS staging path
(`/mnt/overlay/files_{rw,ro}/<target-rel>`) to its container target path. This mirrors how socket
forwards are handled and means file mounts applied later always win over dir-mount content at
the same path.

The `/ez/.files/{rw,ro}` staging directories and their bind mounts are no longer needed and were
removed.
