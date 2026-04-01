# OCI image download with caching

## Context

Currently the container bundle is a hard-coded Alpine minirootfs
prepared by a mise task. We need to pull real OCI images from
registries, cache them, and create per-project CoW bundles for
persistent state across sessions.

## Cache layout

```
~/.ezpez/
  kernel/Image             # extracted from embedded assets (once)
  kernel/initramfs.gz
  images/
    <digest>/              # one per image manifest digest
      rootfs/              # layers merged in order
      manifest.json        # cached manifest
      image_config.json    # cached image config
  projects/
    <hash>/                # sha256(canonical cwd + image digest)
      bundle/
        config.json        # OCI runtime spec (derived from image config)
        rootfs/            # APFS CoW copy of images/<digest>/rootfs
```

## Architecture

```
run()
  ├─ assets::init()                # extract kernel/initramfs to cache
  ├─ oci::resolve(&config.image)   # fetch manifest, get digest
  ├─ oci::ensure_image(digest)     # download layers if not cached
  ├─ oci::ensure_project(digest)   # CoW copy to project dir
  │    └─ clonefile for APFS CoW
  ├─ vm::create(config)            # boot VM, bundle_path = project dir
  ���─ rpc → shell
```

## Plan

### Phase 1: New `oci` module + oci-client dependency

New module `cli/src/oci/mod.rs` with submodules:

**`cli/Cargo.toml`:** add `oci-client`, `flate2`, `tar`

**`cli/src/oci/mod.rs`:** public API:
```rust
pub struct ResolvedImage {
    pub digest: String,
    pub manifest: ImageManifest,
    pub config: ImageConfiguration,
}

pub async fn resolve(image_ref: &str) -> Result<ResolvedImage>
pub async fn ensure_image(resolved: &ResolvedImage) -> Result<PathBuf>
pub fn ensure_project(image_dir: &Path, image_digest: &str) -> Result<PathBuf>
```

**`cli/src/oci/registry.rs`:** wrap oci-client:
- `resolve()` — parse image ref, authenticate (anonymous for public),
  pull manifest + config. Return digest + parsed config.
- `pull_layer()` — download a single layer blob to a file

**`cli/src/oci/layer.rs`:** layer extraction:
- `extract_layers()` — given manifest + layer blobs, extract tar.gz
  layers in order into a merged rootfs/ directory. Handle whiteout
  files (.wh.*) for layer deletion.

**`cli/src/oci/config.rs`:** OCI runtime config generation:
- `generate_config()` — read image config (Cmd, Entrypoint, Env,
  WorkingDir, User) and generate crun-compatible config.json.
  Template: current `sandbox/bundle/config.json` with fields
  replaced from image config.

**`cli/src/oci/cache.rs`:** cache management:
- `cache_dir()` — returns `~/.ezpez`
- `image_dir(digest)` — returns `~/.ezpez/images/<digest>`
- `project_dir(image_digest)` — returns `~/.ezpez/projects/<hash>`
  where hash = sha256(canonical_cwd + ":" + image_digest)
- `cow_copy(src, dst)` — recursive directory copy using
  `libc::clonefile` on macOS for APFS CoW, falls back to regular
  copy if clonefile fails

### Phase 2: Assets refactor

**`cli/src/assets/mod.rs`:** change to cache-based:
```rust
pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

impl Assets {
    pub fn init() -> Result<Self> {
        let dir = cache_dir().join("kernel");
        // Write embedded bytes to dir if not exists
        // Return paths
    }
}
```

No more tempfile — kernel/initramfs persist in cache.

### Phase 3: Wire into main flow

**`cli/src/main.rs` `run()`:**
```rust
let assets = assets::Assets::init()?;
let resolved = oci::resolve(&config.image).await?;
let image_dir = oci::ensure_image(&resolved).await?;
let project_dir = oci::ensure_project(&image_dir, &resolved.digest)?;
// config.bundle_path = project_dir.join("bundle")
```

**Image change detection:**
- Store `<project_dir>/image_digest` file
- On next run, compare stored digest with resolved digest
- If different: prompt user (recreate/keep old/cancel)
- If resolve fails but cache exists: warn and continue

**`cli/src/vm/mod.rs`:** accept assets explicitly instead of
calling extract_assets() internally.

### Phase 4: Remove mise bundle task

- Remove `sandbox/bundle/` directory (build.sh, config.json)
- Remove `build:bundle` mise task
- Remove `.tmp/bundle` from build dependencies
- config.json template moves into Rust code (oci/config.rs)

## Key decisions

- **oci-client crate** (formerly oci-distribution) for registry
  interaction. Async, handles auth, manifests, blob downloads.
- **APFS clonefile** for project copies — `libc::clonefile` is
  available, instant CoW on APFS. Fallback to regular copy.
- **Per-project persistent state** — CoW copy means changes persist
  across sessions but don't affect the cached image.
- **Derive config from image** — read CMD/ENTRYPOINT/ENV/WorkingDir
  from image config instead of hard-coding /bin/sh.
- **Project hash = sha256(cwd + digest)** — different directories
  or different images get separate persistent state.

## Verification

1. `mise run build && mise run ez` with default alpine:latest
2. First run: downloads image layers, creates cache
3. Second run: skips download, uses cached image, CoW project copy
4. Create a file in the shell, exit, re-run → file persists
5. Change `config.image` → prompted about image change
