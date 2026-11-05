# Plan: libkrun backend POC on macOS

Status: phases 0-4 done, see `docs/log/2026-11-02-libkrun-macos-poc.md`.

## Goal

Find out if libkrun can replace Apple Virtualization.framework (VZ) on
macOS arm64, mainly to get guest memory back to the host. Linux (Cloud
Hypervisor) stays as is until the macOS POC passes.

POC passes when:

1. `airlock start -- <cmd>` works with the libkrun backend: shares,
   project disk, vsock RPC (ports 1024, 1025), PTY, network proxy.
2. Host memory footprint of the VM process goes down after the guest
   frees memory (balloon free page reporting, `Replace` mode).
3. vsock throughput is acceptable compared to VZ (measure, no target yet).

## Decisions

- **libkrun version:** 2.0 dev (`main`), pinned to a commit. v1.19.7 has
  no balloon `Replace` mode, and without it macOS does not release
  memory that the guest wrote (`MADV_FREE` only). Use the C API from an
  embedded dylib with `dlopen`. Do not link the crate.
- **Process model:** the VM runs in a child process
  (`airlock __vmm <config>`, hidden subcommand, same binary). libkrun
  `_exit()`s the process on guest shutdown, and `krun_vmm_run` never
  returns. The parent stops the VM with a signal to the child.
- **Kernel:** our own kernel. arm64 `Image` is `RAW` format for libkrun
  (no ELF on aarch64). No libkrunfw.
- **Backends:** keep the current split. `VmConfig` is the backend
  neutral input, `VmHandle` (`vsock_connect`, `stop`) the running VM.
  libkrun is a third `VmHandle` implementation. Backend specific values
  (kernel cmdline console, assets to extract) move into the backend, so
  the common code does not have `cfg` for the backend.
- **Backend selection:** by target. macOS aarch64 gets libkrun, macOS
  x86_64 keeps VZ (libkrun has HVF on arm64 only), Linux keeps Cloud
  Hypervisor for now. During the POC, VZ stays the default on aarch64
  and a setting (e.g. `vm.backend = "libkrun"`) selects libkrun. The
  same guest kernel and initramfs must boot on both backends.

## Phase 0: host access for the agent (done)

The agent ran in a Linux sandbox. A small job server on the macOS host
(dev only, not in the repo) let it build, run airlock with an isolated
home and data dir, and read host process data.

## Phase 1: build libkrun dylib

- New task `build:libkrun` (macOS only): clone libkrun at the pinned
  commit, `cargo build --release -p libkrun --features ffi,blk`, output
  `target/vm/libkrun.dylib`. Do not use `make`: `make all` also builds
  `libkrun_init` (libkrun's own guest init, a Linux musl binary), which
  we do not use. Check if `lld` is still needed.
- Check: dylib exports `krun_vmm_builder_*`, `krun_payload_load_external`,
  `krun_balloon_device_*`.

## Phase 2: kernel config

- arm64 config: add `CONFIG_PAGE_REPORTING`, `CONFIG_MEMORY_BALLOON`,
  `CONFIG_BALLOON_COMPACTION`, `CONFIG_COMPACTION` (now off). Without
  them the guest does not report free pages.
- Check that virtio-mmio devices from the libkrun device tree probe, and
  find the serial device type for the kernel log (`console=` in cmdline,
  `set_kernel_console` panics if `console=` is missing).
- Check guest page block size: `Replace` mode works on 2 MiB aligned
  ranges only (4K pages, `pageblock_order` 9).

## Phase 3: libkrun backend

- `vm/libkrun.rs` (macOS): parent side. Writes the helper config,
  spawns `airlock __vmm`, implements `VmHandle` (`vsock_connect` through
  per-port Unix sockets in the sandbox dir, `stop` with a signal).
- `vm/libkrun/helper.rs`: child side. `dlopen` the dylib from the data
  dir, builds the VM:
  - payload: kernel `Image` (RAW), `initramfs.gz`, cmdline
  - CPUs and memory from `VmConfig`
  - one virtio-fs device per `VmShare` (read-only variant when set)
  - block device for the project disk (raw)
  - vsock, `tsi_features = 0`, ports 1024 and 1025 with `listen = true`;
    unlink stale socket files first (bind errors are only warnings)
  - balloon, free page reporting mode `Replace`
  - serial console to a log file in the sandbox dir
  - no net device, no virtio console
- Assets: embed `libkrun.dylib` (macOS) and extract it with the kernel
  to `<data_dir>/vm/`.
- Signing: the binary already has `com.apple.security.hypervisor`. Dev
  builds are ad hoc signed, so `dlopen` works. Release builds with
  hardened runtime need the dylib signed with the same team ID (or
  `com.apple.security.cs.disable-library-validation`). Solve at
  productization, not in the POC.
- Guest side: replace VZ specific parts if any (e.g. `hvc0` console,
  PCI vs MMIO device names). Expect small changes in `airlockd` init.

## Phase 4: measure

- Memory: in guest, allocate and touch 2-4 GiB, free it, wait. Read
  host `footprint <helper pid>` before, at peak, after. Same run on VZ
  for comparison.
- vsock: copy a large file through the PTY and through the network
  proxy (`dev:network-bench-server`), VZ vs libkrun.
- Known risk: libkrun vsock muxer is one thread, sockets are blocking
  after connect, and a partial `send` is only logged (possible data
  loss). Watch for stalls and corrupt streams under load.

## Known risks (web research 2026-11-02, check during POC)

- `Replace` mode is new (libkrun PR #931, merged 2026-10-20). The 1.19
  backport (#941) is open.
- Pages that the VMM itself writes (virtio-fs and virtio-blk reads) count
  two times in `phys_footprint` (#947). This can hide part of the gain.
- krunkit is killed about every 60 min by macOS resource limits for
  disk writes and CPU wakeups (podman #29084). Run a long session.
- virtio-fs: small file speed 10-30x lower than tmpfs (#924), renamed
  host files ENOENT in guest up to 5 s (#888), shares that sometimes
  return ECONNREFUSED (#910).
- No fuzzing, no audit. Host side confinement is required before release.

## Phase 5: decision and log

Write `docs/log/<date>-libkrun-macos-poc.md` with results. If go,
make a follow-up plan:

- Host side sandbox of the helper (Seatbelt profile: only shares, disk,
  sockets, log). Required, because libkrun treats guest and VMM as one
  trust domain and virtio-fs passthrough does not stop `..` escapes.
- Linux backend with libkrun (KVM): namespaces, `pivot_root`, seccomp,
  landlock; uid/gid mapping test; x86_64 kernel as ELF `vmlinux`.
- Network stack over virtio-net + host side smoltcp, if vsock is slow.
- Intel Mac decision.

## Out of scope

Seatbelt sandboxing, Linux, release signing, removal of the VZ backend.
