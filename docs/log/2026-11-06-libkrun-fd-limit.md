# libkrun VM process file descriptor limit

## Symptom

Parallel load in the guest (uv builds, `pytest -n auto`, 64 parallel Python
processes) failed with `EMFILE` (errno 24): `cannot open shared object
file: Error 24`, `Too many open files`. Guest limits were fine (`ulimit -n`
4096, `fs/file-nr` low). Also `close` returned `EMFILE`, so the error came
from the host side of virtiofs.

## Cause

The VM process inherits the soft `RLIMIT_NOFILE` of the shell that starts
airlock. On macOS the default is 256. The idle VM process already holds
about 163 fds:

* About 130 pipes. libkrun emulates eventfd with pipes on macOS.
* Host fds of guest opens on virtiofs shares. libkrun passthrough keeps one
  host fd for each open guest file, the rootfs layers included. Each guest
  process holds its loader and libc open.

So only about 90 fds were left for guest I/O.

## Fix

At start, the VM process raises the soft limit to
`min(hard limit, kern.maxfilesperproc, 65536)`.

* `kern.maxfilesperproc`: macOS refuses `setrlimit` with `EINVAL` when the
  soft limit is above it, also when the hard limit is `RLIM_INFINITY`.
* 65536: the guest runs untrusted code and controls how many host fds the
  VM process holds. All host processes share `kern.maxfiles` (245760 by
  default). Without a fixed cap, one VM could take 122880 fds (the default
  `kern.maxfilesperproc`), half of the system table, and two VMs could
  fill it. Then all host processes get `ENFILE`. 65536 is about 25% of the
  default table. It is far more than heavy guest load needs (a few
  thousand fds for 64 parallel processes, 10-20k for large builds). At
  the cap only this guest gets `EMFILE`, not the host.

The guest also frees its dentry/inode cache every 10 minutes (see
`2026-04-29-virtiofs-fd-leak.md`). That was for VZ, which kept a host fd
for each cached guest inode. It does not free files that guest processes
hold open, so it does not help with this bug. libkrun seems not to keep
fds for cached inodes (163 fds when idle), but this is not checked in the
libkrun code.

The raise happens before the Seatbelt sandbox applies. A failure is only
logged, because the VM can still run with the low limit.

Only the VM process gets the raise. The airlock process itself does not
hold guest fds.

## Open points

* fds 20-41 of the idle VM process were the same pipe pair duplicated 11
  times. This is likely in libkrun (eventfd emulation or device setup), not
  in airlock. Not examined.
* In the same session, shares showed host uid 503 instead of root, and git
  reported "dubious ownership". Not examined. A possible link: libkrun
  passthrough on macOS reads ownership overrides from xattrs, and an
  `EMFILE` during that lookup can fall back to the host owner.
