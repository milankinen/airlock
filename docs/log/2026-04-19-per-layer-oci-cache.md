# Per-layer OCI cache with preserved whiteouts

Step 2 of the per-layer-cache rollout (see
`2026-04-19-concurrent-layer-downloads.md` for step 1 and the full plan).

## What changed

Every downloaded OCI layer is now extracted twice during `airlock
start`: once into the existing merged rootfs at
`~/.cache/airlock/images/<image-digest>/rootfs/`, and once into a
per-layer directory at `~/.cache/airlock/layers/<layer-digest>/rootfs/`.
The merged tree is still the only thing the guest reads today; the
per-layer trees are the on-disk cache the guest will compose via
overlayfs in step 3.

The per-layer extraction path lives in the new
`layer::extract_layer_cached(digest, tarball)`. It is idempotent (a
`.ok` marker file short-circuits re-extraction) and atomic (extracts
to a sibling `<digest>.tmp` directory, then renames into place). Both
writes happen in parallel across all layers, bounded by the CPU count,
via `tokio::task::spawn_blocking` wrapped in `buffer_unordered`.

## Whiteout preservation

The interesting part of per-layer extraction is that the trees must
stand alone as overlayfs lowerdirs. In the old merged-extraction path
we resolved whiteouts by deleting their targets in already-extracted
layers — that is not an option when each layer is extracted on its own.

The traditional overlayfs whiteout format is a character device with
major/minor 0/0, which requires `CAP_MKNOD` and therefore root on the
host. We extract as an unprivileged user so we use the alternative
`user.overlay.*` xattr format that overlayfs accepts when mounted with
`userxattr`:

- `.wh.<name>` becomes an empty regular file at `<name>` with
  `user.overlay.whiteout="y"`.
- `.wh..wh..opq` sets `user.overlay.opaque="y"` on the parent
  directory.

Guest-side mount flags will be updated in step 3. `userxattr` is
supported by overlayfs since kernel 5.11; our bundled kernel is newer.

## Why keep the merged tree during transition

Step 2 deliberately writes both forms so that the guest keeps working
unchanged while we land the cache changes. Step 5 removes the merged
tree once the guest is switched to overlayfs composition.

## Testing

Four unit tests gated on `target_os = "linux"` (xattr namespace is
Linux-specific) cover regular-file extraction, whiteout→xattr
conversion, opaque-directory marking, and the idempotency short
circuit. They serialize on a static mutex because `cache::cache_dir()`
reads the process-wide `HOME` env var.

A latent bug in the whiteout check was found along the way:
`path_str.contains("..")` matched the legitimate opaque-whiteout
filename `.wh..wh..opq`. Replaced with a component-level check for
`Component::ParentDir`.
