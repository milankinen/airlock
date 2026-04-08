# Plan: Restructure Repository Layout

**Date:** 2026-04-08

## Context

Reorganize the project so all Rust crates live under `crates/`, VM build scripts
move to a top-level `vm/` directory, and all VM build artifacts output to
`target/vm/`. The supervisor binary is renamed to `ezd`. The `sandbox/` directory
is removed entirely.

**virtiofsd note:** GitLab releases for virtiofsd contain source archives only —
no prebuilt binaries for any architecture. Building from source is the only option
and is already what we do.

---

## Target Layout

```
crates/
  common/     ← was protocol/          (Rust crate, package: ezpez-protocol)
  ez/         ← was cli/               (Rust crate, package: ezpez-cli, binary: ez)
  ezd/        ← was sandbox/supervisor (Rust crate, package: ezpez-supervisor, binary: ezd)

vm/           ← top-level VM build dir (NOT a Rust crate)
  kernel/     ← was sandbox/kernel/
  rootfs/     ← was sandbox/rootfs/

target/vm/    ← was sandbox/out/       (gitignored via /target)
```

`sandbox/` is removed entirely. `crates/ez/src/vm/` stays as internal modules —
no new Rust crate is extracted.

---

## Phase 1: Move directories

Using `git mv` to preserve history:

```bash
mkdir -p crates vm
git mv cli      crates/ez
git mv protocol crates/common
git mv sandbox/supervisor crates/ezd
mkdir -p vm
git mv sandbox/kernel vm/kernel
git mv sandbox/rootfs vm/rootfs
rmdir sandbox/out  # artifacts only, already gitignored
rmdir sandbox
```

---

## Phase 2: Update Cargo workspace

**`Cargo.toml`** (root):
- `members = ["crates/ez", "crates/common", "crates/ezd"]`
- `ezpez-protocol = { path = "crates/common" }`

**`crates/ezd/Cargo.toml`** — add supervisor binary name:
```toml
[[bin]]
name = "ezd"
path = "src/main.rs"
```
(Package name `ezpez-supervisor` unchanged; the `-p ezpez-supervisor` cargo flag still works.)

**Verify:** `cargo check --workspace`

---

## Phase 3: Update artifact paths (`sandbox/out/` → `target/vm/`)

All paths shift by one extra `../` because the crates moved one level deeper into `crates/`.

**`crates/ez/build.rs`** (relative to `crates/ez/`):
| Old | New |
|-----|-----|
| `../sandbox/out/Image` | `../../target/vm/Image` |
| `../sandbox/out/initramfs.gz` | `../../target/vm/initramfs.gz` |
| `../sandbox/out/cloud-hypervisor` | `../../target/vm/cloud-hypervisor` |
| `../sandbox/out/virtiofsd` | `../../target/vm/virtiofsd` |
| `cargo:rerun-if-changed=../sandbox/out/...` | same, updated paths |

**`crates/ez/src/assets.rs`** (relative to `crates/ez/src/`):
| Old | New |
|-----|-----|
| `../../sandbox/out/Image` | `../../../target/vm/Image` |
| `../../sandbox/out/initramfs.gz` | `../../../target/vm/initramfs.gz` |
| `../../sandbox/out/cloud-hypervisor` | `../../../target/vm/cloud-hypervisor` |
| `../../sandbox/out/virtiofsd` | `../../../target/vm/virtiofsd` |

**`.gitignore`**: Remove `/sandbox/out` line (covered by `/target`).

**Verify:** `cargo check --workspace`

---

## Phase 4: Update mise tasks

### `mise/tasks/build/kernel`
- MISE sources: `sandbox/kernel/config-*` → `vm/kernel/config-*`, build.sh same
- MISE outputs: `sandbox/out/Image` → `target/vm/Image`
- `mkdir -p sandbox/out` → `mkdir -p target/vm`
- Volume mount: `-v "$PWD/sandbox/kernel/..."` → `-v "$PWD/vm/kernel/..."`
- Volume mount: `-v "$PWD/sandbox/out:/out"` → `-v "$PWD/target/vm:/out"`
- Echo/SIZE lines updated

### `mise/tasks/build/rootfs`
- MISE sources: `sandbox/rootfs/...` → `vm/rootfs/...`, `sandbox/out/supervisor` → `target/vm/ezd`
- MISE outputs: `sandbox/out/initramfs.gz` → `target/vm/initramfs.gz`, rootfs.tar.gz same
- `mkdir -p sandbox/out` → `mkdir -p target/vm`
- Volume mounts: `sandbox/rootfs/` → `vm/rootfs/`, supervisor: `target/vm/ezd`
- Echo lines updated

### `mise/tasks/build/supervisor`
- MISE sources: `sandbox/supervisor/src/**/*.rs` → `crates/ezd/src/**/*.rs`
- MISE outputs: `sandbox/out/supervisor` → `target/vm/ezd`
- `cp target/${TARGET}/release/ezpez-supervisor` → `cp target/${TARGET}/release/ezd`
- Docker build path: `sandbox/supervisor` → `crates/ezd`
- Dockerfile renamed: `sandbox/supervisor/Dockerfile` → `crates/ezd/builder.dockerfile`
- Docker build command updated: `docker build ... crates/ezd` → `docker build -f crates/ezd/builder.dockerfile crates/ezd`
- Inside Docker: `cp /target/.../ezpez-supervisor` → `cp /target/.../ezd`
- `mkdir -p sandbox/out` → `mkdir -p target/vm`
- Output dest: `sandbox/out/supervisor` → `target/vm/ezd`
- Echo line updated

### `mise/tasks/fetch/cloud-hypervisor`
- MISE outputs: `sandbox/out/cloud-hypervisor` → `target/vm/cloud-hypervisor`
- `mkdir -p sandbox/out` → `mkdir -p target/vm`
- `OUTPUT=sandbox/out/cloud-hypervisor` → `OUTPUT=target/vm/cloud-hypervisor`
- Echo line updated

### `mise/tasks/fetch/virtiofsd`
- MISE outputs: `sandbox/out/virtiofsd` → `target/vm/virtiofsd`
- `mkdir -p sandbox/out` → `mkdir -p target/vm`
- `OUTPUT=sandbox/out/virtiofsd` → `OUTPUT=target/vm/virtiofsd`
- Docker volume: `-v "$PWD/sandbox/out:/out"` → `-v "$PWD/target/vm:/out"`
- Echo line updated

### `mise/tasks/build/dev`
- MISE sources: `cli/src/**/*.rs` → `crates/ez/src/**/*.rs`, `cli/Cargo.toml` → `crates/ez/Cargo.toml`, `sandbox/out/*` → `target/vm/*`

### `mise.toml` (vibe task)
- Change `cp -r sandbox/out $tree/sandbox/out` → `mkdir -p $tree/target/vm && cp -r target/vm/. $tree/target/vm/`

---

## Phase 5: Update CI pipeline (`.github/workflows/ci.yml`)

| Old path | New path |
|----------|----------|
| `sandbox/out/Image` | `target/vm/Image` |
| `sandbox/out/initramfs.gz` | `target/vm/initramfs.gz` |
| `sandbox/out/rootfs.tar.gz` | `target/vm/rootfs.tar.gz` |
| `sandbox/out/cloud-hypervisor` | `target/vm/cloud-hypervisor` |
| `sandbox/out/virtiofsd` | `target/vm/virtiofsd` |
| `sandbox/kernel/config-*` | `vm/kernel/config-*` |
| `sandbox/kernel/build.sh` | `vm/kernel/build.sh` |
| download `path: sandbox/out` | `path: target/vm` |

All `upload-artifact` and `download-artifact` `path:` values updated.
Cache key `hashFiles(...)` paths updated.

---

## Phase 6: Update documentation + log entry

**`CLAUDE.md`**: Update any `sandbox/` path references. Binary rename `supervisor` → `ezd`.

**`docs/DESIGN.md`**: Update file path references if any mention `sandbox/out/`.

**Log entry**: `docs/log/2026-04-08-restructure-crates-layout.md`

---

## Verification

```bash
# After phase 2
cargo check --workspace

# After phase 3
cargo check --workspace

# After all phases
mise lint
mise run build:dev   # full build with VM artifacts
```

---

## Files Modified

| File | Change |
|------|--------|
| `Cargo.toml` | workspace members + protocol path |
| `crates/ezd/Cargo.toml` | add `[[bin]] name = "ezd"` |
| `crates/ezd/builder.dockerfile` | renamed from `sandbox/supervisor/Dockerfile` |
| `crates/ez/build.rs` | artifact paths |
| `crates/ez/src/assets.rs` | include_bytes! paths |
| `mise/tasks/build/kernel` | all paths |
| `mise/tasks/build/rootfs` | all paths |
| `mise/tasks/build/supervisor` | paths + binary name |
| `mise/tasks/build/dev` | source paths |
| `mise/tasks/fetch/cloud-hypervisor` | output path |
| `mise/tasks/fetch/virtiofsd` | output path + docker volume |
| `mise.toml` | vibe task sandbox/out ref |
| `.github/workflows/ci.yml` | all sandbox/out refs |
| `.gitignore` | remove /sandbox/out |
| `CLAUDE.md` | path refs |
