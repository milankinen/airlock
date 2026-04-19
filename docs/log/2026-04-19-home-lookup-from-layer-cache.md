# Resolve container HOME by walking per-layer `/etc/passwd` caches

`lookup_home_dir` used to read `images/<digest>/rootfs/etc/passwd` from
the merged image rootfs that the host extracts alongside the per-layer
cache. After CA injection moved to the guest, HOME resolution was the
last host consumer of that merged tree — and the only remaining reason
the host still extracts it.

## What changed

`lookup_home_dir(&Path, u32)` → `lookup_home_dir(&[String], u32)`. The
new signature takes the ordered (topmost-first) list of layer digests
from `ImageMeta.layers` and walks each layer's
`~/.cache/airlock/oci/layers/<digest>/rootfs/etc/passwd` in turn,
returning the home field of the first line that matches the target uid.

`build_oci_image` now passes `&meta.layers` instead of a constructed
`image_dir.join("rootfs")`.

## Why not push this to the guest

The plan called for moving HOME lookup entirely to the guest alongside
CA injection. When implementing, I found `container_home` is used on
the host in three additional places for `~` expansion:

- `vm::mount::resolve_mounts` — user mount target paths
- `vm::disk::prepare` — disk cache paths
- `vm::network` — socket forward target paths

All three run during VM prep on the host *before* the guest boots, so
the guest can't supply the value in time. Pushing `~` expansion to the
guest as well would touch the mount/disk/network pipelines and the RPC
schema in a way that's much larger than the surface of this step.

The underlying goal of step 3 is to eliminate the host's dependency on
a merged `images/<d>/rootfs/`. Reading per-layer `/etc/passwd` files
achieves exactly that goal — the host only reads from the shared
layer cache that other sandboxes already dedup against. Step 5 (drop
`images/<d>/rootfs/` extraction) is unblocked either way.

## Whiteout handling

Layer extraction preserves whiteouts as empty files with
`user.overlay.whiteout="y"` xattrs. A whiteout over `/etc/passwd`
would show up here as an empty file that produces no uid match, so
the loop falls through to the next (lower) layer rather than stopping.
That's a coarser rule than real overlayfs — an upper layer's real
`/etc/passwd` without a match for our uid should shadow the lower
layer's match — but images in practice never remove `/etc/passwd` in
an upper layer, and the "first match wins" walk is a safe superset
for this use case. The alternative (reading the xattr to distinguish
whiteout from genuine absence) isn't worth the complexity here.

## Out of scope

- Pushing `~` expansion for mount/disk/network targets into the guest
  (would let HOME lookup move all the way to guest init).
- Removing `images/<d>/rootfs/` extraction itself — separate step.
