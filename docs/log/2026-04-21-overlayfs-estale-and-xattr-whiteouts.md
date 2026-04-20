# Overlayfs ESTALE on remount + xattr whiteouts not hidden on readdir

## Symptoms

Two distinct bugs that both surfaced after we moved to per-layer
overlayfs composition:

1. **ESTALE on the 2nd sandbox start.** The first boot succeeded; the
   second boot failed with `failed to mount overlayfs: Stale file
   handle (os error 116)`. Rebooting the host did not help; wiping the
   on-disk overlay upperdir did. kmsg drainage surfaced the real
   kernel-side message:
   ```
   overlayfs: failed to verify upper root origin
   ```
2. **Deleted files still visible via `ls` inside the sandbox.** Given
   `dev/xattr.dockerfile` (alpine, `echo tsers > /root/msg` then `rm
   /root/*`), a sandbox booted from that image showed
   `ls: ./msg: No such file or directory` on the first `ls /root`
   and empty on the second. Stat'ing `msg` directly correctly returned
   ENOENT — it was only the directory listing that leaked the name.

## Root causes

### ESTALE

Overlayfs mounted RW defaults to `index=on`. With that flag set, on
mount the kernel records a file-handle-based "origin" xattr on the
upperdir root pointing into the lowerdir, and on every subsequent
mount it re-verifies that handle. Our lowerdirs come from a VirtioFS
share — virtiofsd assigns fresh inode numbers on every VM start, so
the previously recorded handle doesn't resolve after a restart and
the kernel bails with ESTALE before the mount completes.

`xino=off` is needed alongside for the same reason: with
`CONFIG_OVERLAY_FS_XINO_AUTO=y` (which our kernel has) the kernel
encodes a layer-identity tag into upperdir inode numbers, which also
goes stale across virtiofsd restarts.

We don't use `index` for anything — it only affects hardlink
consistency across copy-ups, which we don't rely on.

### xattr-whiteout readdir leak

Our host extractor preserves OCI `.wh.<name>` markers by creating an
empty regular file at `<name>` with `user.overlay.whiteout="y"`.
Kernel overlayfs has two code paths for honoring those:

- **Lookup path (`ovl_lookup` → `ovl_is_whiteout`)** reads the xattr
  directly on each entry. Returns ENOENT for whitened names —
  correct behavior, which is why `stat /root/msg` worked.
- **Readdir path (`ovl_iterate`)** does *not* scan every entry's
  xattrs (that would be prohibitively expensive for large dirs). It
  only engages the xattr-whiteout merge logic when the parent
  directory carries `user.overlay.opaque="x"`. Without that, it
  treats the entry as a plain 0-byte regular, so the "deleted" name
  shows up in directory listings.

`"x"` is the userspace opt-in marker; it is distinct from `"y"`,
which also exists on parent dirs but means "this directory is fully
opaque, hide all lowers" (what `.wh..wh..opq` expands to).

## Fixes

Both in `app/airlock-cli/src/oci/layer.rs` and
`app/airlockd/src/init/linux/overlay.rs`:

1. Extractor now sets `user.overlay.opaque="x"` on the parent dir of
   any xattr whiteout it writes (without clobbering a pre-existing
   `"y"`, which is strictly stronger).
2. Overlayfs mount options now include `index=off,xino=off`.
3. Added a `recent_kmsg_overlay_lines` helper that drains `/dev/kmsg`
   non-blocking and logs any lines mentioning `overlay` whenever
   `mount(2)` returns an error — next time we ship a bug like this,
   the kernel's own diagnostic goes straight to the user's terminal
   instead of being hidden behind a generic `errno`.

## Cache migration

Fix (1) changes the on-disk contract for extracted layers: any layer
extracted before this change lacks the `opaque="x"` xattr on
whiteout parents, and if reused would silently produce the bug we
just fixed. Wiping everyone's cache at upgrade is user-hostile.

Instead we versioned the layer cache:

- `cache::LAYER_FORMAT` is a single `u32` constant (now `2`).
- `cache::layer_key(digest) → "2.<hex>"` replaces raw `<hex>` as the
  on-disk directory name and as the identifier in `OciImage.image_layers`
  (so the guest mount path `/mnt/layers/2.<hex>` matches host-side).
- Image JSON wrapper schema bumped `v1` → `v2` in lockstep.

Upgrade behavior: old `v1` JSONs fail to deserialize (image-level
cache miss → re-resolve); old `<hex>/` layer dirs stay around as
orphans, ignored by everything that consults the cache, and get
reaped the next time `gc::sweep` runs (triggered by `Recreate` /
`airlock rm`). Fresh pulls extract into `2.<hex>/` alongside.

Staging filenames (`.download`, `.download.tmp`, `.tmp`) inherit the
prefix naturally because they're derived from the layer dir's
`file_name()`. `gc::sweep_layers` already uses directory entry names
as liveness keys, so the versioning works through it unchanged —
the only adjustment there was dropping the `digest_name()`
normalization in `collect_live_layers` (entries are already keys).
