# Plan: Unify OCI pull pipeline + drop per-image rootfs + sweep GC

## Context

Steps 1–3 of the earlier per-layer-cache rollout landed: layers are
extracted per-digest under `~/.cache/airlock/oci/layers/<d>/rootfs/`
with preserved whiteouts, and the guest composes overlayfs from those
via `userxattr`. But the transitional crutches from step 2 remain and
have started causing real bugs:

1. **Docker and registry paths still diverge.** Registry downloads per
   layer, extracts into `images/<d>/rootfs/` *and* into `layers/<d>/`.
   Docker exports a merged rootfs and hardlink-mirrors it as a single
   synthetic layer. Two pipelines, two bug surfaces. Docker images also
   don't dedup across images that share a base.
2. **`images/<d>/rootfs/` still gets written** even though the guest
   no longer reads it. The only host-side readers are
   `install_ca_cert` (reads five well-known CA bundle paths) and
   `lookup_home_dir` (reads `/etc/passwd`). Both can be moved.
3. **Image GC is broken.** After the recent
   `~/.cache/airlock/` → `~/.cache/airlock/oci/` cache migration,
   old sandboxes' hardlinks at `.airlock/sandbox/image` dangle at the
   old path. A sibling sandbox that pulls the same image at the new
   path creates it with `nlink == 1`; if the user then picks
   `Recreate`, `gc_unused_image` deletes an image that's genuinely
   still in use elsewhere.
4. **Downloaded tarballs (`images/<d>/layer_<i>.tar.gz`) persist
   indefinitely** — wasted disk after the extraction is done.

This plan unifies everything around the per-layer cache, removes the
merged-tree compose on both sides, moves CA and home-dir lookup to
the guest (eliminating the last host consumers of `images/<d>/rootfs/`),
stages downloads/extractions atomically with post-extraction tarball
cleanup, and replaces per-image GC with a sweep over the full cache.

## High-level changes

### Target cache layout

```
~/.cache/airlock/oci/
  layers/
    <digest>.download.tmp   # in-flight download (deleted on restart)
    <digest>.download       # complete tarball, pending extraction
    <digest>.tmp/           # in-flight extraction (deleted on restart)
    <digest>/               # ready layer (atomic rename from .tmp);
                            # the dir IS the rootfs — no inner rootfs/, no .ok.
                            # presence of the exact-named dir = complete.
  images/
    <digest>/
      meta.json             # { digest, name, layers: [top…bottom] }
      image_config.json     # kept for env/cmd/uid/gid
```

`images/<d>/rootfs/` is gone. Per-image tarballs (`layer_<i>.tar.gz`)
are gone. The `rootfs/` subdir and `.ok` marker inside each layer dir
are gone — atomic rename from `<digest>.tmp` to `<digest>` is the
completion signal; guest overlayfs lowerdirs become `/mnt/layers/<d>`
directly.

### Unified per-layer pipeline

Both Docker and registry paths converge on:

```
for each layer digest in the image:
    if layers/<d>/ exists → skip (directory presence IS the marker)
    else:
        acquire tarball:
            registry: stream pull_layer → layers/<d>.download.tmp → rename .download
            docker:   stream `docker save`; for each blobs/sha256/<d> entry,
                      if layers/<d>/ missing, write to layers/<d>.download.tmp
                      → rename .download. Discard entries whose layers are cached.
        extract layers/<d>.download → layers/<d>.tmp/ → atomic rename
          to layers/<d>/
        remove layers/<d>.download
```

Docker-specific pre-step: stream the single `docker save` tar once,
buffer `manifest.json`, write non-cached blobs to `layers/<d>.download`
atomically. Then extract each referenced layer via the shared
`extract_layer_cached` path.

Layer digest for the docker path comes from the blob filename
(`blobs/sha256/<hex>`), which is the content-addressable tar digest.
The image digest comes from `manifest.json`'s `Config` field.

### CA certs moved to the guest

`install_ca_cert` and the `sandbox_dir/ca/` directory are deleted.
The CA PEM bytes are passed to the guest via a new RPC field:

```capnp
caCert @N :Data;   # PEM bytes; empty when CA injection not needed
```

The guest, after mounting overlayfs, walks the five known CA bundle
paths (`etc/ssl/certs/ca-certificates.crt`, …) and for each that
exists in the lower stack, reads the file, appends the CA, writes the
combined bytes back at the same path (upperdir gets the copy-up for
free). When none of the paths exist in the image, the CA is still
written to `etc/ssl/certs/ca-certificates.crt` so e.g. `curl` can be
pointed at it via `SSL_CERT_FILE`.

The `ca` virtiofs share is removed from `prepare_shares` and the
`mount_virtiofs("ca")` call is dropped from `init/linux.rs`. The CA
overlay is no longer an overlayfs lowerdir — it's a regular file on
the upper that was edited by guest init before the container started.

### Home-dir lookup moved to the guest

`lookup_home_dir` is deleted from the host. The RPC stops carrying
host-resolved `HOME=…` in the env list; guest init, after overlayfs
is mounted, reads `/etc/passwd` from the composed rootfs, resolves
home for the given uid, and prepends `HOME=<home>` before exec'ing
the container process.

This removes the final reason the host needed access to `/etc/passwd`
from a merged rootfs.

### Sweep-based GC

`gc_unused_image` is replaced with `oci::gc::sweep()`:

```rust
pub fn sweep() -> Result<()> {
    // Pass 1: for each images/<d>, if meta.json nlink <= 1, remove the dir.
    //   (nlink > 1 means some sandbox holds the hardlink ref.)
    //   Collect live layer digests from surviving meta.json files.
    // Pass 2: for each layers/<d>, if digest not in the live set, remove.
    //   Also unconditionally remove stray .download / .download.tmp / .tmp entries.
}
```

Triggers:

- On `ImageChangeAction::Recreate` (replacing today's `gc_unused_image`
  call).
- On `airlock rm <sandbox>` if it already runs GC (check and replace).

We do **not** run sweep on every `prepare()` — it would race against
sibling sandboxes in the middle of starting up (their hardlinks may
not yet exist). Binding GC to user-initiated remove actions keeps it
safe.

**Bug fix side effect**: because `sweep` consults all images in the
new-path cache at once, a sibling sandbox's hardlink at the correct
new-path meta.json is sufficient to protect the image. Dangling
old-path hardlinks (from the cache migration) are no longer relevant
because the old path itself is gone.

### Hardlink-creation robustness

Today `prepare()` silently logs and continues if
`std::fs::hard_link(meta_path, sandbox_image)` fails — which leaves
the sandbox with no GC protection. Make this a hard error with a
clear message (typical failure is a cross-device link, which we never
expect since both paths are under `$HOME`).

### Runtime whiteouts

No host-side change needed. The guest already mounts overlayfs with
`userxattr`, and the upperdir lives on our ext4 virtio-blk disk
(which supports user xattrs). When a process inside the sandbox
deletes a file from a lower layer, overlayfs creates a whiteout in
the upperdir encoded as an empty file + `user.overlay.whiteout="y"`
xattr — same format the host extractor uses for `.wh.*` entries.
Subsequent `readdir` calls suppress the name. So deletions propagate
correctly across both host-baked layer whiteouts and runtime
sandbox-writable upperdir whiteouts.

## File-level changes

### Host CLI

- `app/airlock-cli/src/oci.rs`
  - `ImageMeta` keeps current shape.
  - `prepare()`: remove CA install call, remove rootfs existence check
    from fast path (check `meta.json` + every `layers/<d>/` directory
    instead), make hardlink failure fatal.
  - `ensure_image()`: delete both divergent paths. New single flow:
    1. Resolve layer list + image config (per source).
    2. For each missing layer, in parallel (bounded), call
       `layer::ensure_layer_cached(digest, fetch_fn)`.
    3. Write `meta.json` + `image_config.json`.
    - Remove `mirror_merged_rootfs_into_layer_cache` entirely.
    - Remove `layer::extract_layers` call.
  - `build_oci_image`: drop `rootfs` field; drop `container_home`
    field; keep `uid`/`gid`/`cmd`/`env` (without HOME).
  - Delete `lookup_home_dir`.
  - Replace `gc_unused_image` with a call to `oci::gc::sweep()`.

- `app/airlock-cli/src/oci/layer.rs`
  - Delete `extract_layers` (merged-tree path, no longer used).
  - Add `ensure_layer_cached(digest, fetch_tarball)`: idempotent,
    does the `.download.tmp` → `.download` → `.tmp` → `<digest>/`
    dance (no `.ok`, no inner `rootfs/` — atomic rename into
    `<digest>/` IS the completion marker), removes `.download` on
    success. `fetch_tarball` is a closure `(dest: &Path) -> Result<()>`
    that writes a tarball to `dest` (the `.download.tmp` path).
  - Rewrite existing whiteout-preserving extraction to extract
    directly into the layer dir (no inner `rootfs/`).
  - Unit tests cover: idempotent re-entry, partial `.tmp` left over
    from a killed run, tarball removal on success, stray
    `.download.tmp` cleanup.

- `app/airlock-cli/src/oci/docker.rs`
  - `save_and_extract` → `save_layer_tarballs`:
    - Signature: `(image_ref: &str, layer_needed: impl Fn(&str) -> bool)
      -> Result<(Vec<String>, OciConfig)>` returning the ordered layer
      digest list + parsed config.
    - Streams `docker image save`; buffers `manifest.json`; for each
      `blobs/sha256/<d>` entry, if `layer_needed(d)` is true, writes
      to `layers/<d>.download.tmp`, renames to `.download`. Otherwise
      discards bytes.
    - No rootfs extraction inside this module.
  - `image_config_dest` parameter removed; config bytes are returned.

- `app/airlock-cli/src/oci/registry.rs`
  - `pull_layer` caller passes a `.download.tmp` path; caller renames
    to `.download` on success (symmetric with docker path).

- `app/airlock-cli/src/oci/gc.rs` (new)
  - `pub fn sweep() -> anyhow::Result<()>` as described.

- `app/airlock-cli/src/project.rs`
  - Delete `install_ca_cert`.
  - Expose `ca_cert: &[u8]` so the RPC layer can forward it.

- `app/airlock-cli/src/vm.rs`
  - Drop the `ca` share and `sandbox_dir/ca/` handling in
    `prepare_shares`.
  - Drop the `install_ca_cert` call.
  - `OciImage` no longer carries `rootfs` or `container_home`; tighten
    call sites.

- `app/airlock-cli/src/rpc/supervisor.rs`
  - Set the new `caCert` field from the project's CA PEM.
  - Stop sending `HOME=…` in env.

### RPC schema

- `app/airlock-common/schema/supervisor.capnp`
  - Add `caCert @N :Data;` on the `start` params. Regenerate.

### Guest

- `app/airlockd/src/init.rs`
  - `MountConfig` gains `pub ca_cert: Vec<u8>`.

- `app/airlockd/src/init/linux.rs`
  - Drop the `mount_virtiofs("ca")` call.
  - Drop `/mnt/ca` from the overlayfs `lowerdir=` construction.
  - Switch lowerdir entries from `/mnt/layers/<d>/rootfs` to
    `/mnt/layers/<d>` (directory is the rootfs).
  - After overlayfs is mounted at `/mnt/overlay/rootfs`:
    - If `ca_cert` is non-empty, iterate the five CA bundle paths
      relative to the rootfs; for each path whose file exists in the
      lower stack, read-append-write into the overlay. Also write a
      fallback at `etc/ssl/certs/ca-certificates.crt` when none of
      the known paths existed.
    - Resolve HOME from the composed `/etc/passwd` via uid; prepend
      `HOME=<home>` to the env list before spawning the container
      init.

- `app/airlockd/src/rpc.rs`
  - Parse `caCert` bytes into `MountConfig.ca_cert`.

### Docs

- `docs/DESIGN.md` — update cache layout + CA injection + GC
  description.
- `docs/manual/src/usage/starting-sandbox.md` — note the unified
  per-layer cache; drop any mention of the per-image rootfs.
- `docs/log/<today>-unified-oci-pipeline.md` — implementation log
  per commit-worthy step.

## Implementation order

Each step is a self-contained commit.

1. **Sweep GC + hardlink failure is fatal.** Lowest risk, fixes the
   current "old image deleted" bug without touching the pipeline.
   `oci.rs` + new `oci/gc.rs`.
2. **CA to guest via RPC.** `supervisor.capnp` + `init/linux.rs` +
   `rpc.rs` + `project.rs` + `vm.rs` + `rpc/supervisor.rs`.
3. **Home-dir lookup to guest.** Drop `lookup_home_dir` and
   `container_home`; guest resolves HOME post-mount.
4. **Unified pull pipeline + staged downloads + tarball cleanup.**
   Registry path first (simpler — digests known upfront). Then the
   docker path (stream-split).
5. **Drop `images/<d>/rootfs/` extraction entirely.** At this point
   nothing still reads it. Remove from `ensure_image` and from the
   fast-path check. `meta.json` alone is the image marker.

Steps 1–3 can each land without the others. Steps 4–5 should land
together to avoid leaving a half-migrated state on disk.

## Verification

- `mise run lint` + unit tests pass at every step.
- Unit coverage: `ensure_layer_cached` (idempotent, cleans tarball on
  success, survives killed mid-extract, cleans stray
  `.download.tmp`), `gc::sweep` (preserves live images via hardlinks,
  prunes orphaned layers + stray staging dirs).
- Manual regression:
  1. Fresh cache start with a multi-layer image → layers download
     concurrently, image boots, `$HOME` inside container is correct,
     HTTPS to a proxied host works.
  2. Second project pulling an image that shares layers with (1) →
     only non-shared layers download.
  3. `Recreate` of (1) to a different image → old image gone,
     layers shared with (2) retained.
  4. `airlock rm` on a sandbox whose image has no other refs →
     image + its unique layers deleted, shared layers retained.
  5. Kill airlock mid-pull → stray `.download.tmp` / `.tmp` / leftover
     `.download` files exist; next `airlock start` cleans and
     re-downloads/extracts only what's missing.
  6. Docker-daemon image that shares a base with a registry-pulled
     image → shared base extracted only once. Confirm by inspecting
     `layers/` before and after the second pull.

## Out of scope

- `cache_oci_layers=false` opt-out setting (can be re-added later).
- Per-layer GC triggered on every `prepare()` (race-prone).
- `airlock image ls` / `airlock image gc` CLI commands.
