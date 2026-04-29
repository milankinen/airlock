# Periodic FITRIM in the supervisor

The project disk image is sparse but doesn't reclaim space when files
are deleted inside the sandbox — `du -h .airlock/sandbox/disk.img`
keeps growing to a high-water mark over a project's lifetime, with no
release short of `airlock rm`. Run `fstrim` on `/mnt/disk` from inside
the VM every 10 minutes so the host file shrinks live.

### Why a periodic trim, not mount-time `discard`

ext4 supports a `discard` mount option that issues a TRIM on every
block free. The major distros moved away from it years ago because the
per-delete overhead is steady and noticeable — the now-canonical
pattern is a periodic batched trim. ext4 tracks already-trimmed
extents so subsequent invocations after the working set settles are
near-no-ops, and 10 minutes is a sensible cadence for an interactive
sandbox: the user sees the image shrink within a few minutes of
deleting a build directory, with no per-write tax in between.

A shutdown-only trim was tempting (one call, no timer) but loses the
"shrinks live" property — long-running sandboxes (an agent left going
for hours, a VM the user attaches to repeatedly via `airlock exec`)
would still inflate to a high-water mark until quit.

### `FITRIM` ioctl, not the `fstrim` binary

The `fstrim` userspace utility may not exist in arbitrary base images
— the bare minimum that always works is the kernel ioctl. ~30 lines
of Rust calls `FITRIM` directly:

```rust
const FITRIM: libc::c_ulong = 0xc018_5879;  // _IOWR('X', 121, fstrim_range)

#[repr(C)]
struct FstrimRange { start: u64, len: u64, minlen: u64 }

let dir = std::fs::File::open("/mnt/disk")?;
let mut range = FstrimRange { start: 0, len: u64::MAX, minlen: 0 };
unsafe { libc::ioctl(dir.as_raw_fd(), FITRIM, &raw mut range) };
// range.len now holds the count of bytes trimmed
```

`libc` doesn't expose `FITRIM`, but the encoding is the same across
the architectures airlock targets, so we hardcode the constant.
`start = 0`, `len = u64::MAX` means "from the beginning of the
filesystem to the end"; `minlen = 0` lets the kernel pick its default
minimum extent size.

### Lifecycle and crash-safety

The trim task is `tokio::task::spawn_local`'d after `init::setup`
succeeds and runs for the lifetime of the supervisor. First fire is
*after* the first interval, not at boot — at boot there's nothing
discardable yet.

If the CLI kills the VM mid-trim, ext4 is unaffected: `FITRIM` only
reads the free-space bitmap and issues hole-punch requests for blocks
that are already free. It writes no fs metadata, doesn't touch the
journal, and stays out of the data path. Discards already completed
on the host are persistent (good); discards still in flight in the
virtio queue just get dropped and the corresponding ext4 blocks
remain free and allocatable. Same robustness as killing during any
other normal fs operation.

### Best-effort, hypervisor-dependent

Whether the trim actually reclaims host bytes depends on the
hypervisor passing `VIRTIO_BLK_F_DISCARD` through to the host backing
file. Cloud Hypervisor and Apple Virtualization both do; older or
unusual stacks may not. If the discard never reaches the host file
the trim becomes a no-op and the image keeps its high-water mark —
the manual notes `airlock rm` as the always-works fallback.

Errors from the ioctl are logged at `warn!` and swallowed; we never
want a misbehaving hypervisor to take the supervisor down.

### Out of scope

- User-configurable interval. 10 minutes is a defensible default; if
  it ever becomes a sticking point, a single `[disk]` knob is a small
  follow-up.
- Trimming on shutdown in addition to the periodic loop. The next
  periodic run handles whatever the user just deleted; adding a
  shutdown-time trim doubles the implementation surface for a
  marginal payoff (catching the last <10 min of churn).
