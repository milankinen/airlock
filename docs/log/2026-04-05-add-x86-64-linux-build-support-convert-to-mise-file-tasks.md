# Add x86_64/Linux build support, convert to mise file tasks

Preparing for Linux host support by making the build pipeline
architecture-aware and converting build tasks to mise file tasks.

### Build tasks → file tasks

Moved all build tasks from inline `mise.toml` definitions to standalone
scripts in `mise/tasks/build/`. File tasks are easier to maintain for
multi-line scripts and allow proper shell tooling (shellcheck, editor
support). Non-build tasks (test, format, lint, ez) remain inline since
they're one-liners.

### Architecture detection

- **Kernel**: `build.sh` now receives `ARCH` env var. On x86_64 hosts
  it uses `config-x86_64` and copies `bzImage`; on arm64 it keeps the
  existing `config-arm64` and copies `Image`. Output is always
  `sandbox/out/Image` regardless of arch.
- **Supervisor**: Detects `uname -m` to pick the right musl target
  (`x86_64-unknown-linux-musl` vs `aarch64-unknown-linux-musl`).
  Defaults to Docker build on all platforms; set
  `SUPERVISOR_BUILD_HOST=true` for native toolchain (used in CI where
  deps are pre-installed).
- **Dev CLI**: Skips macOS codesign on Linux.
- **x86_64 kernel config**: New `config-x86_64` mirrors the arm64
  config (namespaces, cgroups, virtio, vsock, netfilter, virtiofs) with
  x86-specific settings (KVM_GUEST, PARAVIRT, 8250 serial).

### CI workflow

Replaced hardcoded supervisor build commands in CI with
`SUPERVISOR_BUILD_HOST=true mise run build:supervisor`.

### Lint fixes

Added `#[cfg(target_os = "macos")]` gate on `CString` import and
`#[allow]` annotations for `VmConfig` fields and `vm::start` async —
these are unused on Linux until a VM backend is added.
