# Unify OCI pull pipeline; drop merged rootfs; stage downloads atomically

After the earlier per-layer rollout, two transitional crutches were
still in the tree:

- Registry and docker paths diverged. Registry downloaded layers to
  `images/<d>/layer_N.tar.gz`, extracted into `images/<d>/rootfs/`
  *and* into `layers/<d>/`. Docker `save`d into a merged rootfs and
  then hardlink-mirrored that whole tree into `layers/<d>/` as one
  synthetic layer — two pipelines, two bug surfaces, and no layer
  dedup across docker-sourced images.
- `images/<d>/rootfs/` was still written even though the guest
  composes overlayfs from the per-layer cache. The only remaining
  host readers of the merged tree (CA injection, home-dir lookup)
  were both removed in earlier commits this session.
- Downloaded tarballs (`layer_N.tar.gz`) stuck around after
  extraction — wasted disk.

This commit lands steps 4 and 5 of the per-layer plan together
(per the plan's own guidance, to avoid leaving a half-migrated cache
on disk).

## New cache layout

```
~/.cache/airlock/oci/
  layers/
    <digest>.download.tmp   # in-flight download
    <digest>.download       # complete tarball, pending extraction
    <digest>.tmp/rootfs/    # in-flight extraction
    <digest>/               # finished: rootfs/ + .ok marker
  images/
    <digest>/
      meta.json             # digest, name, ordered layer digests
      image_config.json     # env/cmd/uid/gid
```

Each transition is an atomic rename so a crash at any point leaves a
state the next run can either clean up (`gc::sweep`) or resume from
(`ensure_layer_cached`).

## Unified per-layer pipeline

`layer::ensure_layer_cached(digest, fetch)` is the single entry point
for both sources. Fast path: `<digest>/.ok` exists → return. Resume
path: `<digest>.download` exists (previous crash or pre-staged tarball)
→ skip fetch, extract. Otherwise: `fetch(&tmp)` populates
`<digest>.download.tmp`, rename to `.download`, extract, remove
tarball on success.

Registry: each layer's fetcher is a `pull_layer` call that streams
the blob to the `.download.tmp` path. `pull_layer` no longer owns
rename logic — the caller hands it an explicit temp path and renames
on success.

Docker: `docker::save_layer_tarballs` streams `docker image save` once
and splits blobs into per-layer staging files. Blobs come in as
`blobs/sha256/<hex>` entries without knowing which are layers vs. the
config; we stage every blob to `<hex>.download.tmp`, then after
`manifest.json` has been parsed we classify:

- Config blob → read into memory (persisted to `image_config.json`),
  staging file deleted.
- Cached layer → staging file deleted.
- Needed layer → rename `<hex>.download.tmp` → `<hex>.download`, ready
  for `ensure_layer_cached` to extract (its `fetch` closure is
  unreachable because the tarball is already present).
- Stray / duplicate blob → deleted.

This gets docker-sourced images into the shared layer cache properly
— identical base layers across images now extract only once.

## Side effect: `ensure_layer_cached` resumes partial work

Before: a crash between "download the tarball" and "finish extraction"
forced a full re-download on the next run. Now `<digest>.download`
persists the complete tarball across the atomic rename into
`<digest>/`, so a crash mid-extract only costs the extraction, not
the bytes on the wire.

## Image GC and fast path

`prepare()`'s fast-path cache check drops the `images/<d>/rootfs/`
existence test — the image is considered ready when `meta.json` exists
and every listed layer has its `.ok` marker. The `KeepOld` branch
likewise no longer requires rootfs.

`gc::sweep` from step 1 already handles stray `.download.tmp`,
`.download`, and `.tmp` entries under `layers/`, so the new staging
names Just Work with existing GC.

## Tests

`ensure_layer_cached` gets five unit tests covering: fresh extract,
whiteout xattr preservation, opaque-directory xattr, idempotent
no-op second call (fetch closure must not be invoked when `.ok`
exists), resume-from-staged-download (fetch not invoked when
`.download` exists), and cleanup of stale `.download.tmp` from a
killed previous run.
