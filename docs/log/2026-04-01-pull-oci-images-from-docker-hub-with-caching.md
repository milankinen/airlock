# Pull OCI images from Docker Hub with caching

### What

Replace the static Alpine minirootfs bundle with real OCI image
pulling from Docker Hub. Images are cached per-digest, project
bundles use APFS copy-on-write for persistent state across sessions.

### Cache layout

```
~/.ezpez/
  kernel/Image, initramfs.gz     # extracted once from embedded assets
  images/<digest>/rootfs/        # downloaded + extracted image layers
  projects/<hash>/bundle/        # CoW copy of image rootfs + config.json
```

### New modules

- `cli/src/oci/registry.rs` — `oci-client` wrapper: resolve image
  ref → manifest + config + digest, pull layer blobs. Uses
  `linux/arm64` platform resolver.
- `cli/src/oci/layer.rs` — extract tar.gz layers in order into
  merged rootfs. Handles OCI whiteout files (.wh.*) for deletions.
- `cli/src/oci/config.rs` — generate OCI runtime config.json from
  image config (CMD, ENTRYPOINT, ENV, WorkingDir, User).
- `cli/src/oci/cache.rs` — cache directory management, APFS
  `clonefile` CoW copy with fallback to regular copy.
- `cli/src/project.rs` — project hash from canonical CWD.
- `cli/src/assets/mod.rs` — refactored to cache-based (no more
  tempfile, kernel/initramfs persist in `~/.ezpez/kernel/`).

### Key decisions

- **`oci-client` crate** for registry interaction — async, handles
  auth, manifests, blob downloads. Platform resolver set to
  `linux/arm64` since the VM is ARM64.
- **Project hash = sha256(canonical_cwd)** — image digest stored
  separately as a file for change detection. Different directories
  get independent persistent state.
- **APFS clonefile** for project bundle copies — instant CoW on
  APFS, fallback to regular copy on other filesystems.
- **Derive config.json from image** — CMD, ENTRYPOINT, ENV,
  WorkingDir, User read from image config. Fallback to /bin/sh
  if none specified.
- **Removed mise build:bundle task** — bundle preparation is now
  runtime (on first `ez` run), not build time.
