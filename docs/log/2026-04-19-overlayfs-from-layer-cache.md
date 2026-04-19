# Compose overlayfs in the guest from the shared layer cache

Step 3 of the per-layer-cache rollout. The guest no longer reads a
per-image merged rootfs tree; it composes its rootfs with one overlayfs
lowerdir per image layer, drawn from the single shared layer cache at
`~/.cache/airlock/oci/layers/`.

The OCI cache was also moved out of the top-level `~/.cache/airlock/`
into a dedicated `oci/` subdirectory alongside `images/` and `layers/`,
so future non-OCI cache kinds don't collide with image/layer digests.

## What changed

- `supervisor.capnp`: `start` gains `imageLayers :List(Text)` — the
  ordered (topmost-first) digest list for the image being launched.
- Host CLI:
  - Persists the layer digest list in `images/<d>/meta.json` as
    `layers`, reversed from the OCI manifest's bottom→top order so it
    matches overlayfs' top→bottom `lowerdir=` order.
  - Docker-daemon images become a single synthetic layer keyed by the
    image digest. After `docker image save` merges into a rootfs, we
    hardlink-mirror (`cp -al`) it into `layers/<image-digest>/rootfs/`
    so the guest composes it the same way.
  - `OciImage` carries `image_layers: Vec<String>`; the
    `Supervisor.start` client forwards it to the guest.
  - `vm::prepare_shares` drops the per-image `base` virtiofs share and
    adds a single shared `layers` share pointing at
    `cache::layers_root()` (read-only). The guest picks which subtrees
    matter by digest.
  - The fast-path cache hit in `prepare()` now also verifies every
    listed layer's `.ok` marker, so a partially-populated layer cache
    forces a re-pull instead of booting into a broken overlay.
- Guest (`airlockd`):
  - Drops `mount_virtiofs("base")`, adds `mount_virtiofs("layers")` at
    `/mnt/layers`.
  - `assemble_rootfs` builds `lowerdir=` from the supplied digest list,
    prepends `/mnt/ca` if present, and mounts with `userxattr` so
    overlayfs honors the `user.overlay.whiteout` /
    `user.overlay.opaque` xattrs set by the host extractor in step 2.

## Why reversed order

OCI manifests list layers bottom→top (base first, image-author last).
overlayfs wants the topmost (highest-priority) layer first in
`lowerdir=`. Doing the reverse at write-time keeps `meta.json`
interpretable without a "note: reverse this" comment at every read
site.

## Why a single shared share, not per-image

Sharing `layers/` as a single read-only virtiofs tag means:

- No share churn between starts of sibling projects — the host doesn't
  relaunch virtiofsd per image. A cached layer used by a second image
  is already accessible in the existing share.
- virtiofsd's cost scales with the number of shared dirs, not with how
  many files live under each. One share covers every image.

The guest picks which subtrees are load-bearing by digest, so the
unused entries cost nothing beyond an open file descriptor per layer
dir entry.

## Why `userxattr`

The traditional overlayfs whiteout format is a `c 0 0` character
device, which needs `CAP_MKNOD` — not available on macOS hosts and
inconvenient to grant for host-side extraction. The xattr format is
the blessed alternative and works as an unprivileged user when the
mount is flagged `userxattr`. Requires kernel >= 5.11; our bundled
kernel is well past that.

`virtiofsd` is already launched with `--xattr`, so user xattrs make
it through the guest boundary intact.

## Compatibility

- `images/<d>/rootfs/` is still populated in step 2's merged-tree form.
  The guest no longer reads it, but `install_ca_cert` still does, and
  it provides a safe fallback if step 3 needs to revert. Step 5 of the
  plan removes it.
- `meta.json` gains a `layers: Vec<String>` field with
  `#[serde(default)]`. Caches without it are treated as incomplete and
  re-pulled on next start (controlled by the fast-path check in
  `prepare()`).

## What's not covered here

- Per-layer GC when an image is removed (step 4).
- The `cache_oci_layers` opt-out setting (step 4).
- Removing `images/<d>/rootfs` extraction entirely (step 5).
