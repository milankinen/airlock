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
checksum of `libkrun.dylib`. There is no cargo feature: all macOS
arm64 builds contain libkrun. `mise run build:libkrun` (a thin wrapper of
`app/vm-libkrun/build.sh`) makes `target/vm/libkrun.dylib`. The dev and
release builds depend on it.

## Changes from the PoC

* Backend selection by the `vmm` setting, not `AIRLOCK_VM_BACKEND`.
* Each asset file has its own stamp file (`<file>.checksum`): the embedded
  checksum plus the device, inode, size and mtime of the extracted file.
  Other airlock versions share the asset dir with other stamp files. Their
  rename makes a new inode, so the stamp shows the change and the file
  extracts again. The backend extracts the library in `boot_vm`. The
  `include_bytes!` is not in test builds, so `cargo test` on macOS works
  without the dylib.
* The backend does not show in `airlock info` or the start summary.
* Sandbox process command renamed to `airlock internal vmm`.
* VM process entry: `airlock internal vm-libkrun` (hidden clap
  subcommand). `main` starts it before all other init (completions,
  panic hook, context, logs). Same as the PoC: config JSON on stdin, exit
  on stdin EOF, empty env, log pipe with a 16 MiB cap.
* `dlopen` through `libc` with `RTLD_NOW`, not `libloading`. No new
  dependency, and all symbols bind before the sandbox applies.
* The sandbox process sends real paths (no symlink in any part). The VM
  process gives the same strings to libkrun and to the policy, and the
  apply refuses a path that is not its real path (see Seatbelt).
* The VM process closes all inherited fds except 0-2 first. All error
  exits use `_exit`, because the VM threads keep running on a normal exit.
* No console device, and no `console=` on the kernel command line. airlock
  never read the console, and the supervisor sends its logs and the kernel
  messages over vsock. The guest init now opens fds 0-2 on `/dev/null`
  when the kernel has no console for it (all backends).
  `libkrun.log` has only libkrun messages.
* Unexpected stop: if the VM process stopped before a stop request, the
  vsock connect (boot) fails at once (no retries, typed `VmStopped` error)
  with `the libkrun VM process stopped. Log: <path>` and the last 2 KiB of
  `libkrun.log`, without control characters. Measured: 0.1 s after a kill
  of the VM process, 0.4 s for a libkrun error at build. A guest that
  hangs while the VM process lives (for example a bad kernel) still waits
  for the 12 s retry limit. A stop after the boot with a
  non-zero exit status gives the same error in the sandbox `airlock.log`.
  The client shows "lost the process in the sandbox" and "the sandbox did
  not stop correctly", as for other backends.
* Kept from the PoC: `VmShare.immutable` (cache policy `Always` for the
  layers share), `kill_group -> bool`, `remove_old` in `vm/mod.rs`,
  `CONFIG_COMPACTION=y` in the arm64 kernel config.

## Seatbelt

On macOS, `krun_vmm_builder_build` starts the vCPU threads, and the guest
runs at once (`resume_vcpus` does nothing). Thus the VM process applies the
policy after the `dlopen` and the log setup, before it makes any device.
If the apply fails, the VM process stops with an error. The guest never
runs without the sandbox.

Policy (`deny default`):

* read-only shares: `file-read*` on the subpath
* read-write shares: `file-read* file-write*` on the subpath
* kernel and initramfs: `file-read*` on the file
* disk image: `file-read* file-write*` on the file
* vsock sockets: `network-bind` on the socket path and `file-write-create`
  on the socket file
* `sysctl-read` of `kern.hv_vmm_present` (the hypervisor library at the
  build)

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
  process uses only real paths.
* The system log does not show the denies of the VM process reliably
  (the kernel drops and merges reports). Thus the rules were found with an
  experiment profile `(deny default (with send-signal SIGKILL))`: each deny
  kills the VM process, and the boot shows it. With the apply before the
  build, a boot needs only the rules above. A boot with a workload (share
  create, rename, delete, 300 MB disk write, network) found only one other
  deny: `mach-priv-host-port` from the hypervisor library at vCPU start. A
  process without root cannot get this port, and the VM runs normally
  with the deny. The policy does not allow it.
* Rules that the experiments removed as not used: other `sysctl-read`
  names, `system-socket`, `network-inbound`, `file-read-metadata` and
  `file-write-unlink` on the socket, `/.vol` rules.
* The VM process can create an entry at a socket path. The sandbox process
  connects only if the entry is a socket file (not a symlink to an other
  host socket). A swap between the check and the connect stays possible
  in theory.

## Other results

* `tests/vm/libkrun.bats` (macOS arm64 only): boot, project share
  read/write, read-only mount. It sets a short `data_dir`, because the
  socket paths must fit the 104-byte macOS limit.
* VZ still boots.
