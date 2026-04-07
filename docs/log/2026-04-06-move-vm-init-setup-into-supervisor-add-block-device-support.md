# Move VM init setup into supervisor, add block device support

The shell init script was causing race conditions and hangs under libkrun,
especially with cache disk attached. Moved all VM setup (virtiofs mounts,
networking, cache disk, DNS) from the init script into the supervisor binary.

### Why

Under libkrun, the init script runs via init.krun's chroot/exec. When the
cache disk was attached, mkfs.ext4 took time and the host vsock connection
would arrive before the supervisor started, causing RST and connection
failures. Moving the setup into the supervisor means it happens AFTER the
RPC connection is established — errors get proper logging via LogSink.

### Changes

- **RPC schema**: extended `start()` with `shares`, `epoch`, `hostPorts`,
  `hasCacheDisk` fields
- **supervisor/init.rs**: new module performing all VM setup in Rust:
  clock sync (libc::clock_settime), virtiofs mounts (libc::mount),
  networking (ip/iptables via Command), cache disk (blkid/mkfs/mount),
  DNS (resolv.conf). All errors logged via tracing → LogSink RPC.
- **Init script**: reduced to minimal mounts (proc/sys/dev/cgroup) + exec
  supervisor. No config reading, no virtiofs, no networking, no cache.
- **libkrun**: built with `make BLK=1` for block device support, added
  `krun_add_disk` FFI call. Removed env var passing (`build_env`).
- **CLI**: passes init config via RPC, computes epoch/shares/cache_disk
  at the caller level.
