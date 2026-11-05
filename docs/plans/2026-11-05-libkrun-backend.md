# Plan: libkrun VM backend for macOS arm64

Follow-up of `docs/plans/2026-11-02-libkrun-macos-poc.md`. PoC code is on
branch `libkrun-poc`.

## Goal

Ship libkrun as a selectable VM backend on macOS arm64. VZ stays the
default. libkrun stays pinned to the libkrun 2.0 commit of the PoC.

## Decisions

- **Setting:** top-level `vmm = "apple-virt" | "cloud-hypervisor" | "libkrun"`.
  The settings parser accepts all values on all hosts. Default: `apple-virt`
  on macOS, `cloud-hypervisor` on Linux. `AIRLOCK_VM_BACKEND` goes away.
- **Runtime backends:** `vm::VmmBackend` trait, used as `dyn VmmBackend`.
  The sandbox process loads the backend first in
  `sandbox::run_sandbox_process`. If the host does not support the backend,
  it stops with an error.
- **Layout:** one submodule per backend, each with the same structure:

  ```
  vm/apple_virt/{mod.rs, apple_virt.rs}
  vm/cloud_hypervisor/{mod.rs, cloud_hypervisor.rs}
  vm/libkrun/{mod.rs, libkrun.rs, ...}
  ```

  `mod.rs` does the `#[cfg]` gating and gives the loader. The
  implementation file has no top-level cfg for the platform.
- **cfg:** `vm_libkrun` = macOS + aarch64. No cargo feature: all macOS
  arm64 builds (dev, release) contain libkrun, as Linux builds contain
  Cloud Hypervisor.
- **Assets:** each backend embeds and extracts its own assets
  (cloud-hypervisor + virtiofsd, libkrun dylib) in `boot_vm`. `vm::start`
  extracts and resolves the kernel and initramfs. Shared extraction code
  (lock, atomic write, checksum per asset file) lives in `vm/assets.rs`.
- **VMM process:** `airlock internal vm-libkrun` (in `cli/cmd_internal.rs`).
- **Hardening:** Seatbelt through the C API (`sandbox_init_with_parameters`
  with `extern "C"`). Deny by default. Allow: read for the read-only
  shares, the kernel and the initramfs, read and write for the read-write
  shares and the disk image, bind on the vsock Unix sockets that the
  sandbox process connects to. The VM process applies the policy before
  the VM build, because the build starts the guest. Paths go in as profile
  parameters, not as profile text.

## Work units

1. **Restructure (A1):** backend layout, `VmmBackend` trait, `vmm` setting,
   early backend load, asset move. Apple and Cloud Hypervisor only.
2. **libkrun port (A2):** port PoC code into `vm/libkrun/`, the internal
   command, `build:libkrun` in dev builds, kernel config.
3. **Seatbelt (B1):** profile, C API binding, escape tests.
4. **Packaging (B2):** CI dylib build and cache, release build, signing.
5. **UI and docs (B3):** backend in `airlock info` and the VM start
   summary, manual pages.
6. **Unexpected stop (B4):** clear error when the VMM process stops
   without a request.

## Verification

- Linux: `mise run lint`, `mise run test`.
- macOS: `mise run lint:macos` from Linux checks the compile. The user
  runs the macOS runtime tests (boot, escape tests, long session).

## Not in this plan

Default switch to libkrun, Linux libkrun, released libkrun version,
performance work.
