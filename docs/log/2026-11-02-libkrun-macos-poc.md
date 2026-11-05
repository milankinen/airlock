# libkrun VM backend POC on macOS

## Why

The Apple Virtualization framework (VZ) never gives guest memory back to
macOS. The VM process keeps its peak footprint until the VM stops: a
long-running sandbox with a 32 GiB VM showed 32.0 GiB physical footprint
on the host. VZ has no fix for this.

libkrun 2.0 (unreleased, `main`) has a free page reporting mode `Replace`
that gives the memory back on macOS. The guest reports free pages, and
the VMM unmaps them from the hypervisor, maps fresh memory over them and
maps them back. The default mode uses `MADV_FREE`, and macOS does not
release guest-written pages with it, even under memory pressure. The
stable 1.19.x line does not have `Replace` (backport PR #941 is open).

An earlier try (2026-04, libkrun 1.17 on Linux) failed on missing
`trusted.*` xattrs in virtio-fs, TSI socket hijacking, the implicit
console, and the dlopen packaging. In 2.0 nothing is implicit (no TSI, no
console unless added), and our overlay upperdir is on the ext4 disk now,
so virtio-fs xattrs do not matter.

## What was built

Dev builds on macOS arm64 contain a libkrun backend next to VZ. VZ stays
the default. `AIRLOCK_VM_BACKEND=libkrun` selects libkrun. Release and CI
builds do not change.

* `mise run build:libkrun` clones libkrun at a pinned 2.0 commit and
  builds only the VMM library with the C API
  (`cargo build -p libkrun --features ffi,blk`). `make` would also build
  libkrun's own guest init, which needs a musl target and is not used.
* Cargo feature `libkrun` (on in `build:dev` on Darwin). `build.rs` sets
  the `vm_libkrun` cfg for macOS aarch64 with the feature. The dylib is
  embedded and extracted with the other VM assets.
* The VM runs in a child process (`airlock __vmm`), because
  `krun_vmm_run` never returns and libkrun `_exit()`s the process when
  the guest stops. The parent sends the config as JSON on stdin and keeps
  stdin open. The child exits on stdin EOF, because macOS has no parent
  death signal. The child starts with an empty environment.
* Devices: one virtio-fs device per share (read-only variant when set),
  raw block disk, vsock without TSI, balloon in `Replace` mode, rng,
  virtio console. No network device. MMIO transport.
* vsock: libkrun listens on one Unix socket per guest port and connects
  the host to the guest port. libkrun accepts the host connection before
  the guest answers, and a refused port shows only as EOF. The parent
  waits up to 200 ms for data or EOF and retries on EOF. Only the
  supervisor connect waits the full time (once per boot), because on the
  network port the guest sends first. The socket path length is checked
  against the macOS 104-byte limit before start.
* Guest console and libkrun logs go through a pipe to
  `<sandbox>/libkrun.log`, capped at 16 MiB, so the guest cannot fill
  the host disk.
* Shares have a backend-neutral `immutable` flag. The `layers` share sets
  it, and libkrun uses the `Always` cache policy for it, so the guest
  keeps image file data across opens. Other shares use the default
  `Auto` (close-to-open). `Always` is not safe for the project dir,
  because the host edits it.
* Same kernel and initramfs as VZ. arm64 config: `CONFIG_COMPACTION=y`,
  so free memory forms larger blocks that the guest can report.
  `CONFIG_PAGE_REPORTING` was already on (the balloon driver selects it).
  4K pages give 4 MiB reports, which fit the 2 MiB rule of `Replace`.
* Kernel format: aarch64 `Image` is `RAW` for libkrun. libkrun does not
  accept ELF on aarch64, nor bzImage on x86_64 (ELF vmlinux only).
* The ad hoc signed dylib loads in dev builds. The binary already has
  `com.apple.security.hypervisor`.

## Benchmarks

Apple silicon Mac, macOS 26.7.1. VM: 10 CPUs, 32 GiB. Alpine
guest. The network and virtio-fs tests ran 2 times per backend (values
are averages), the memory test 1 time.

### Memory

Measure the VMM process footprint with `vmmap --summary <pid>`
("Physical footprint"). The `footprint` tool does not see guest memory
mapped into the hypervisor and shows about 60 MB.

Workload: write 4 GiB into a tmpfs in the guest, wait, remove it.

| Backend | Before | Peak | 60 s after `rm` |
|---|---|---|---|
| libkrun (`Replace`) | 865 MiB | 4.8 GiB | 870 MiB (in about 15 s) |
| VZ | 826 MiB | 4.8 GiB | 4.8 GiB |

### Network over vsock

GET 2 GiB from a host server (`localhost:8080` exposed with
`[network.ports] host`), 3 runs each.

| Backend | Time | Rate |
|---|---|---|
| VZ | 18.2 s | ~113 MB/s |
| libkrun | 19.9 s (18.9-22.3) | ~103 MB/s |

Both stop at about the same rate, thus the airlock network proxy is the
probable limit, not vsock.

### virtio-fs (project share)

| Test | VZ | libkrun |
|---|---|---|
| write 1 GiB file | 2.56 s | 1.73 s |
| read 1 GiB file (cold) | 0.48 s | 0.47 s |
| read 1 GiB file (warm) | 0.52 s | 0.48 s |
| write 10000 x 4 KiB (`tar x`) | 14.0 s | 17.7 s |
| stat 10000 files (`find`) | 0.42 s | 0.33 s |
| read 10000 files (cold) | 1.84 s | 2.05 s |
| read 10000 files (warm) | 1.85 s | 2.13 s |
| delete 10000 files | 3.20 s | 5.06 s |

Reference, ext4 sandbox disk (both backends about the same): small file
write 3.7 s, delete 0.08 s. Warm reads on the share are not faster than
cold reads, because the default cache policy drops cached data on open.

## Findings and limits

* File locks are not shared between host and guest. The libkrun virtio-fs
  server does not implement `GETLK`/`SETLK`/`SETLKW` (stubs return
  `ENOSYS`) and does not offer `FUSE_POSIX_LOCKS`/`FUSE_FLOCK_LOCKS`, so
  the guest kernel keeps locks local. VZ does not share them either.
* libkrun does not keep the guest in a shared dir: the macOS passthrough
  builds `/.vol/<dev>/<ino>/<name>` paths without `..` checks, and libkrun
  has no self-sandbox. The maintainers treat guest and VMM as one trust
  domain. A Seatbelt profile for the VMM process is required before
  release.
* Intel Macs: libkrun has HVF on arm64 only. The x86_64 macOS build must
  keep VZ.
* Pages that the VMM itself writes (virtio-fs and block reads) count two
  times in the footprint (libkrun issue #947).
* Known libkrun virtio-fs issues on macOS: renamed host files can show
  as missing in the guest for up to 5 s (attribute and entry timeouts),
  shares that sometimes return `ECONNREFUSED` (#910).
* The libkrun vsock muxer is one thread, sockets are blocking after
  connect, and a partial `send` is only logged. No problem showed up in
  the tests.
* The 2.0 API is unreleased and has no fuzzing or audit. `Replace` mode
  was merged on 2026-10-20.

## Next

* Seatbelt sandbox for the `__vmm` process.
* Long session test (krunkit gets killed after about 60 min by macOS
  resource limits, podman #29084).
* Decide on Linux: libkrun (KVM) with namespaces, seccomp and landlock,
  or keep Cloud Hypervisor.
* Release packaging: sign the dylib with the release identity, CI build.
