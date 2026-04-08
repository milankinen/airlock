# Restructure Repository Layout

Reorganized the project directory structure for clarity and consistency.

## Changes

### Rust crates under `crates/`

All Rust packages moved under a `crates/` umbrella:

- `cli/` → `crates/ez/` (CLI binary, package: `ezpez-cli`)
- `protocol/` → `crates/common/` (shared Cap'n Proto types, package: `ezpez-protocol`)
- `sandbox/supervisor/` → `crates/ezd/` (VM supervisor, package: `ezpez-supervisor`)

The supervisor binary is renamed from `ezpez-supervisor` (default name) to `ezd`
via an explicit `[[bin]] name = "ezd"` entry in its Cargo.toml.

### VM build scripts under `vm/`

Kernel and rootfs build scripts moved to a top-level `vm/` directory:

- `sandbox/kernel/` → `vm/kernel/`
- `sandbox/rootfs/` → `vm/rootfs/`

### VM artifacts under `target/vm/`

Build artifact outputs moved from `sandbox/out/` to `target/vm/`, which is already
covered by the existing `/target` gitignore rule. The `sandbox/` directory is
removed entirely.

- `sandbox/out/Image` → `target/vm/Image`
- `sandbox/out/initramfs.gz` → `target/vm/initramfs.gz`
- `sandbox/out/cloud-hypervisor` → `target/vm/cloud-hypervisor`
- `sandbox/out/virtiofsd` → `target/vm/virtiofsd`
- `sandbox/out/ezd` (was `supervisor`) → `target/vm/ezd`

### Dockerfile renamed

`sandbox/supervisor/Dockerfile` → `crates/ezd/builder.dockerfile` — clearer name
for a builder image (not a runtime image).

### virtiofsd: prebuilt binaries investigation

Checked the virtiofsd GitLab releases page. The project only publishes source
archives (no prebuilt binaries for x86_64 or aarch64). Building from source
remains the only option and is unchanged.

## Files updated

- `Cargo.toml` — workspace members, protocol path
- `crates/ezd/Cargo.toml` — added `[[bin]] name = "ezd"`
- `crates/ez/build.rs` — artifact paths updated
- `crates/ez/src/assets.rs` — `include_bytes!` paths updated
- `mise/tasks/build/{kernel,rootfs,supervisor,dev}` — all paths updated
- `mise/tasks/fetch/{cloud-hypervisor,virtiofsd}` — output paths updated
- `mise.toml` — vibe task sandbox/out reference updated
- `.github/workflows/ci.yml` — all artifact paths updated
- `.gitignore` — removed `/sandbox/out` (covered by `/target`)
- `CLAUDE.md` — schema path reference updated
