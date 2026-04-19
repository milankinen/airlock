# Collapse `images/<digest>/{meta,image_config}.json` to a single file

The per-image cache directory held two JSON files: `meta.json`
(digest + name + ordered layer list) and `image_config.json` (the raw
OCI config — cmd, env, uid/gid). Every `ensure_image` call read both,
merged them into an `OciImage`, and returned that to the caller. The
cache was carrying the pieces; the callers were re-assembling the
product.

Flip it: build the `OciImage` once at write time — when every input
is already in hand — and persist the serialized result directly. The
cache entry becomes a single file at `images/<digest>` holding the
full `OciImage`, wrapped in a schema envelope:

```json
{
  "schema": "v1",
  "image_id": "sha256:…",
  "name": "docker.io/library/alpine:latest",
  "image_layers": ["sha256:…", "sha256:…"],
  "container_home": "/root",
  "uid": 0,
  "gid": 0,
  "cmd": ["/bin/sh"],
  "env": []
}
```

The `"schema": "v1"` tag is deliberately a string, not a number — it
lets us later migrate to `#[serde(tag = "schema")]` with an
internally-tagged `CachedImage` enum (e.g. `V1(OciImageV1)`,
`V2(OciImageV2)`) without reshuffling the on-disk payload.

Write is atomic: serialize → `<digest>.tmp` → `rename` → `<digest>`.
Hard-link GC transfers cleanly: `sandbox/image` is a hard-link to
the file (not to an inner `meta.json`), and `gc::sweep` checks
`nlink() <= 1` on each file under `images/`.

Fast path in `prepare()` simplifies from "read meta.json → read
image_config.json → build OciImage → check rootfs" down to
"`read_cached_image(sandbox/image)` → check every listed layer's
`rootfs/` exists." `OciImage` gains `name: String` (previously only
on the stripped `meta.json`) and `Clone + Serialize + Deserialize`;
`ImageMeta`, `read_sandbox_image_meta`, and the separate
`image_config.json` write are gone.

`build_oci_image` no longer reads from disk — it takes
`(image_id, name, ordered_layers, image_config)` directly from the
caller, which already has them after layer resolution.

## Short-circuits that fall out of the new layout

Once `images/<digest>` is the canonical cache entry, two redundant
code paths go away:

**`ensure_image` digest-keyed fast path.** Today the per-sandbox fast
path in `prepare()` checks the hardlinked entry in the sandbox dir,
which is keyed by *name*. But the cache is content-addressable by
digest, so a sibling project that pulled the same image (possibly
under a different tag) has already done the work. `ensure_image` now
reads `images/<digest>` directly; if present and every listed layer
is on disk, return the cached `OciImage`. Skips `docker image save`
and the full registry pull entirely. Refreshes the stored name on
hit so the per-sandbox fast path agrees for subsequent runs.

**`docker::save_layer_tarballs` inline discard.** The function used
to write every blob streaming out of `docker image save` to
`<hex>.download.tmp`, then after parsing `manifest.json` delete the
ones whose layer was already cached. For a 2 GB base image that had
already been extracted, that's 2 GB of pointless disk writes. Now
the streaming loop checks `layer_dir(digest).rootfs.is_dir()` per
blob; on a hit the bytes are drained to `io::sink()` instead. Config
blobs can't collide (different content → different sha256), so the
inline check is unambiguous.

Stale caches from the previous layout (`<digest>/meta.json` +
`<digest>/image_config.json`) are harmless: the sweep GC treats them
as non-files (`metadata.is_file()` is false for a directory) and
skips them, and subsequent pulls rewrite entries at the new path.
