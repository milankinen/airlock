# VM review fixes (macOS-testable part)

A full review of the VM backends and guest init found these problems. This
entry fixes the ones that macOS (Apple Virtualization, libkrun) can test.
The other findings are in a separate list for later work.

## Apple Virtualization: fds without close-on-exec

The console pipes and the vsock fds (`dup` of the connection fd) had no
`FD_CLOEXEC`. Host child processes that airlock starts during a session
(host MCP servers, the notifier) got copies. After airlock closed its copy
of a vsock fd, the connection stayed open, and the guest got no EOF.

Fix: the pipes come from `std::io::pipe` (close-on-exec), and each `dup`
is `fcntl(F_DUPFD_CLOEXEC)`.

## Apple Virtualization: console output not read

Nothing read the guest-to-host console pipe. With a debug log level the
kernel writes much console output. A full pipe blocks the console writes
of the guest. A thread now reads the pipe and drops the data until EOF.

## Guest: read-only bind mounts were writable

`mount(MS_BIND | MS_RDONLY)` ignores `MS_RDONLY` on a new bind mount. Thus
the read-only dir mounts, the read-only file mounts and the masks were
writable at the guest level. For shares, the host server still refused
writes. A mask is a bind of a directory on the sandbox disk, so writes
into a masked path went to the disk. A second `mount(MS_REMOUNT | MS_BIND |
MS_RDONLY)` now makes them read-only. Writes into a masked path now fail
with `EROFS`.

## Guest: disk format decision

Guest init formatted `/dev/vda` when the `blkid` output had no "ext4".
Busybox `blkid` returns 0 with no output both for an empty disk and when
it cannot read the disk. A read error thus formatted the disk and erased
the overlay upper layer and the caches.

Fix: no `blkid`. Guest init reads the first 64 KiB of the disk:

* ext4 superblock magic (0xEF53 at byte 1080): mount.
* All zeros: airlock made the disk as a sparse file of zeros, so it is
  new: format.
* Other data or a read error: stop the boot with an error.

## Rules

* The error text for a missing extracted asset is now in `messages.rs`.
* A missing `SAFETY:` comment in the Cloud Hypervisor backend.
