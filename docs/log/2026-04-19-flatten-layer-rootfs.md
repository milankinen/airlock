# Extract layer contents directly into `layers/<digest>/`

The per-layer cache used to stage extraction as
`layers/<digest>.tmp/rootfs/`, atomic-rename to `layers/<digest>/`,
and leave readers walking `<digest>/rootfs/<…>` for every access. The
inner `rootfs/` directory was load-bearing as a commit marker back
when per-image `meta.json`/`.ok` files coexisted with layer dirs —
its presence meant "this layer's extraction finished." Now that
`layers/<digest>/` itself only exists via the atomic rename from
`<digest>.tmp/`, the directory's presence is already the commit
marker. The inner `rootfs/` is a redundant hop.

Flip it: extract the tarball directly into `<digest>.tmp/` (no
inner `rootfs/`), rename to `<digest>/`. The layer contents live at
the root of the layer dir. Every reader — the guest's overlayfs
lowerdir composition, the host's `/etc/passwd` walk in
`lookup_home_dir`, the inline cached-layer check in
`docker::save_layer_tarballs`, the registry path's `to_fetch` filter,
the per-layer `ensure_layer_cached` fast path — stops joining
`"rootfs"` onto the layer path.

## Blast radius

- Host: `cache::layer_dir(d)` callers now check `.is_dir()` on the
  layer dir itself rather than `.join("rootfs").is_dir()`. Same
  number of syscalls, one fewer path component.
- Guest: overlayfs `lowerdir=` entries shift from `/mnt/layers/<d>/rootfs`
  to `/mnt/layers/<d>`. Similarly, the container-spec lookup walks
  `/mnt/layers/<d>/<rel>` instead of `/mnt/layers/<d>/rootfs/<rel>`.
  Virtiofs share is unchanged — the host still shares
  `~/.cache/airlock/oci/layers` as a single `layers` mount.
- On-disk: cached layers written under the old layout have an extra
  `rootfs/` directory; `is_dir()` still returns `true` on the outer
  dir, so the fast path claims "already cached" but the contents are
  one level too deep, producing empty overlayfs lowerdirs. Users
  purge `~/.cache/airlock/oci/layers/` manually (approved — cache is
  rebuildable, the alternative is carrying a migration shim
  indefinitely).
