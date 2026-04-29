# Bound the virtiofs proxy's host FD count

The host process backing each airlock VM (`com.apple.Virtualization`
on macOS) accumulates an unbounded number of open file descriptors as
the guest browses the shared project tree. With two VMs running over
a workday on a large monorepo, FD counts of 90k–150k per VM have been
observed, eventually pushing other host processes into `EMFILE`.

This isn't a leak — virtiofs sharing requires the host proxy to keep
a real FD open for every guest VFS entry that's cached (page cache,
dentry/inode cache, mmap, open FD anywhere in the guest). Linux only
evicts those caches under memory pressure, and airlock's default
half-of-host-RAM allocation means pressure rarely arrives. Cached
inodes plateau at "however much fits in 8–16 GB of slab", and the
host proxy holds proportional FDs for the entire plateau.

Native CLI tools don't show this because they follow open-read-close;
agents (Claude Code etc.) walk huge trees and the kernel keeps every
visited inode cached.

### Two complementary mitigations

Both ship together, applied in `disk::setup` and the periodic
maintenance task:

1. **`vm.vfs_cache_pressure = 200`** at boot, written to
   `/proc/sys/vm/vfs_cache_pressure` after the project disk mounts.
   The default of `100` weights dentry/inode reclaim equal to page
   cache reclaim; doubling it makes the kernel prefer slab when
   any reclaim opportunity arises. Free, no behaviour change for
   the workload, but only kicks in when there's *some* pressure.

2. **`echo 2 > /proc/sys/vm/drop_caches`** every 10 minutes from
   the supervisor's existing periodic maintenance loop (the same
   loop that runs `FITRIM`). Forces eviction of slab even without
   pressure. `2` drops only dentries and inodes — not the page
   cache — so file *contents* aren't re-fetched from the host;
   only metadata walks (`readdir`, `stat`) re-populate the slab on
   next access.

(1) alone wasn't enough — with `cache_pressure=200` and 16 GB of
RAM, the slab ceiling is still well into the hundreds-of-thousands
range. (2) is the actual lever; (1) makes the steady-state lower
between drops.

### Why slab-only, not full `drop_caches=3`

`drop_caches=3` also drops the page cache, which would force the
guest to re-fetch file *contents* over virtiofs on the next read.
For a workload that revisits the same files repeatedly (an editor,
a build with a hot ccache) that's measurable I/O. `2` is the
minimum that addresses the FD problem (the host proxy's FDs back
the *inode* cache, not the page cache).

### Why a periodic drop, not mounting with `discard` semantics

There's no equivalent of mount-time `discard` for the FD problem —
the kernel offers no "evict slab as soon as any entry is freeable"
mode. A periodic forced drop is the canonical pattern; the same
shape and cadence as the existing `FITRIM` task makes them a
natural pair. Combining them into one tokio task (10-minute
interval, `FITRIM` then `drop_caches=2` serially) keeps the
implementation simple and the user-visible operations correlated:
both reclaim sandbox resources back to the host on the same beat.

### Crash-safety

`drop_caches` is the same kind of operation as natural reclaim
under memory pressure — the kernel handles it on every Linux
system every day. Killing the VM mid-drop is a no-op: the slab
walk is in-RAM, no on-disk state changes. Same robustness story
as `FITRIM`.

### Out of scope

- Per-mount `cache=metadata` virtiofs option. Apple's
  Virtualization framework doesn't expose that knob, and on Cloud
  Hypervisor we'd be giving up performance to fix something the
  guest-side mitigations already solve.
- Configurable interval. 10 minutes matches the existing
  maintenance cadence; if it ever needs tuning, a single `[disk]`
  knob is a small follow-up.
- Driving the drop from the host instead of the guest. The host
  has no way to ask the guest's kernel to reclaim — the guest
  owns its VFS.
