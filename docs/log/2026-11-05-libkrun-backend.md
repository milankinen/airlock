# libkrun VM backend

Follow-up of `2026-11-02-libkrun-macos-poc.md` and the plan
`docs/plans/2026-11-05-libkrun-backend.md`. The PoC code (branch
`libkrun-poc`) is now a selectable backend: `vmm = "libkrun"` in
`~/.airlock/settings.toml`. VZ stays the default on macOS.

## Module layout

```
vm/libkrun/mod.rs        cfg gating (`vm_libkrun`) and the loader
vm/libkrun/libkrun.rs    backend: asset extraction, VM process start,
                         vsock connect, stop, unexpected stop
vm/libkrun/vmm.rs        VM process: libkrun API, devices, Seatbelt apply,
                         krun_vmm_run
vm/libkrun/seatbelt.rs   Seatbelt policy and C API binding
vm/libkrun/tests/        escape test of the Seatbelt sandbox
```

`build.rs` sets `vm_libkrun` for macOS aarch64 targets and gives the
checksum of the `libkrun` asset group. There is no cargo feature: all macOS
arm64 builds contain libkrun. `mise run build:libkrun` (a thin wrapper of
`app/vm-libkrun/build.sh`) makes `target/vm/libkrun.dylib`. The dev and
release builds depend on it.

## Changes from the PoC

* Backend selection by the `vmm` setting, not `AIRLOCK_VM_BACKEND`.
* The library is its own asset group. The backend extracts it in
  `boot_vm`. The `include_bytes!` is not in test builds, so `cargo test` on
  macOS works without the dylib.
* VM process entry: `airlock internal vmm-libkrun` (hidden clap
  subcommand). `main` starts it before all other init (completions,
  panic hook, context, logs). Same as the PoC: config JSON on stdin, exit
  on stdin EOF, empty env, log pipe with a 16 MiB cap.
* `dlopen` through `libc` with `RTLD_NOW`, not `libloading`. No new
  dependency, and all symbols bind before the sandbox applies.
* The VM process resolves the share and socket paths before it gives them
  to libkrun (see Seatbelt).
* Unexpected stop: if the VM process stopped before a stop request, the
  vsock connect (boot) fails with `the libkrun VM process stopped. Log:
  <path>` and the last 2 KiB of `libkrun.log`. A stop after the boot with a
  non-zero exit status gives the same error in the sandbox `airlock.log`.
  The client shows "lost the process in the sandbox" and "the sandbox did
  not stop correctly", as for other backends.
* Kept from the PoC: `VmShare.immutable` (cache policy `Always` for the
  layers share), `kill_group -> bool`, `remove_old` in `vm/mod.rs`,
  `CONFIG_COMPACTION=y` in the arm64 kernel config.

## Seatbelt

The VM process applies the policy right before `krun_vmm_run`. At that
point libkrun has loaded, the devices exist, and the kernel, the initramfs
and the disk image are open. If the apply fails, the VM process stops
with an error. The guest never runs without the sandbox.

Policy (`deny default`):

* read-only shares: `file-read*` on the subpath
* read-write shares: `file-read* file-write*` on the subpath
* vsock sockets: `network-bind` on the socket path and `file-write-create`
  on the socket file

Paths go in only as parameters.

### Verification (macOS 26.7.1, Apple silicon)

* `sandbox_check(pid)` of the running VM process gives 1.
* Escape test (`cargo test vm::libkrun`): reads in the share and through
  `/.vol/<dev>/<ino>` of a share file work. Denied: a file outside, `..`
  out of the share, absolute and relative links to outside, `/.vol` of the
  outside file, `/.vol/<share ino>/..`, a write in a read-only share, a TCP
  connect, a bind of a socket that is not in the list. A bind of the listed
  socket works.
* The sandbox denies an access through a symlink in the path (it needs a
  read of the link, for example `/var` -> `/private/var`). Thus the VM
  process resolves the paths first.
* The system log does not show the denies of the VM process reliably
  (the kernel drops and merges reports). Thus the rules were found with an
  experiment profile `(deny default (with send-signal SIGKILL))`: each deny
  kills the VM process, and the boot shows it. A boot with a workload
  (share create, rename, delete, 300 MB disk write, network) found only one
  deny: `mach-priv-host-port` from the hypervisor library at vCPU start. A
  process without root cannot get this port, and the VM runs normally
  with the deny. The policy does not allow it.
* Rules that the experiments removed as not used: `sysctl-read` (all
  reads come before the apply), `system-socket`, `network-inbound`,
  `file-read-metadata` and `file-write-unlink` on the socket, the disk
  image and kernel file rules (open before the apply), `/.vol` rules.
* libkrun opens the share roots and binds the vsock sockets after the
  apply (the device activation), so these rules are necessary.

## Other results

* `tests/vm/libkrun.bats` (macOS arm64 only): boot, project share
  read/write, read-only mount. It sets a short `data_dir`, because the
  socket paths must fit the 104-byte macOS limit.
* VZ still boots.
