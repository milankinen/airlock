# Single-binary with embedded kernel and rootfs

### What

Kernel and rootfs are now built locally via `docker run` and embedded
into the `ez` binary with `include_bytes!`. No runtime downloads —
ship one binary that boots a full Linux VM.

### Build pipeline

Kernel (6.18.3) is built from source using PUI PUI Linux's defconfig
(`sandbox/kernel/config-arm64`). Rootfs is an Alpine 3.23 minirootfs
with a custom `/init` script, packed as a cpio initramfs. Both are
built inside ephemeral Docker containers and output to `sandbox/out/`.

`mise run build` orchestrates: kernel → rootfs → cargo build + codesign.
All tasks have `sources`/`outputs` for incremental rebuilds (2ms no-op).

### Cargo workspace

Project restructured into a workspace:

- `cli/` — the `ez` binary (macOS host, Apple Virtualization)
- `supervisor/` — agent that will run inside the VM (built for Linux
  via Docker with `rust:alpine`)

Workspace root manages shared dependency versions.

### Key decisions

#### Embedded assets via include_bytes!

VZLinuxBootLoader requires file URLs, so embedded bytes are written
to a `tempfile::TempDir` at startup. The temp dir is kept alive for
the process lifetime via the `AssetPaths._tmp` field. This adds ~12MB
to the binary (7MB kernel + 5MB rootfs) but eliminates all runtime
download code (reqwest, sha2, tar, flate2, cpio all removed).

#### docker run over docker build

Build scripts (`sandbox/*/build.sh`) are mounted into ephemeral
`alpine:3.23` containers. No Dockerfiles, no image layers to manage.
Cargo cache for supervisor builds uses a named Docker volume
(`ezpez-cargo-cache`) to avoid re-downloading crates.
