# File-sync destination anchoring

## The bug

The host-side file-mount sync watches `overlay/files/rw/<key>` and
mirrors changes back to the user's host source path. The overlay side
was already locked down with `O_NOFOLLOW`, but the destination side
re-resolved the source path on every event: `std::fs::metadata(source)`,
`std::fs::rename(tmp, source)`, `open(tmp)`-where-`tmp` is
`source.with_file_name(...)`. Each of those resolves intermediate
directory components at syscall time.

The project mount is read-write — that's intentional. So a process
inside the sandbox can replace any directory component leading up to
a mount's host source with a symlink, since the project tree is
shared via virtiofs. Concrete scenario:

1. `[mounts.bashrc] source = "/Users/you/project/cfg/.bashrc"` and
   the project root is shared.
2. Sandbox starts; sync watches the overlay.
3. From inside the sandbox: `mv project/cfg project/cfg.real;
   ln -s /etc project/cfg`.
4. Sandbox writes to its overlay file.
5. Host sync wakes up, calls `rename(tmp, source)` where `source` is
   `/Users/you/project/cfg/.bashrc` — `cfg` is now a symlink to
   `/etc`, so the new file lands at `/etc/.bashrc` (owned by you, but
   with overlay-controlled contents).

Anything `[mounts.X].source` points to that has an attacker-writable
ancestor anywhere along the path is exploitable for an arbitrary host
write of whatever the sandbox put in the overlay.

## The fix

Pin each source's parent directory by FD at sandbox startup, then
write through `*at()` syscalls relative to that FD instead of ever
re-resolving the source path.

New `SyncDest { parent_fd, basename, display }`:

- `parent_fd` opened with `O_DIRECTORY | O_NOFOLLOW` once when the
  watcher is built. After this point, the FD references the original
  directory inode forever (even if the path that *led* to it is
  swapped, renamed, or unlinked).
- Subsequent operations target that FD: `fstatat` for the hard-link
  freshness check, `linkat` from the overlay FD into the parent FD
  for re-establishing the link, `renameat(parent, tmp, parent, name)`
  for the atomic swap, `openat(parent, tmp, O_CREAT|O_EXCL|O_NOFOLLOW)`
  for the copy fallback's temp file, `unlinkat` for cleanup.

The path-component lookup happens exactly once — at startup, before
any sandbox process exists. Symlink swaps after that don't change
what the FD refers to.

## What if the parent is gone at startup?

`SyncDest::open` fails → the mount is logged and skipped. No silent
"sync into the path-resolved-target" fallback; if we can't pin the
intended location we don't write anywhere. This is stricter than the
previous behaviour (which would have happily synced to whatever the
path resolved to) but is the right default for a security boundary.

## What if the parent is swapped after startup?

The FD still references the original directory. Writes land there
correctly — sync keeps doing exactly what it was set up to do. The
attacker's symlinked replacement target is untouched. This is the
property the test in `file_sync.rs::tests` asserts: the redirect
target stays empty *and* the originally pinned parent receives the
new payload.

The behaviour is not "abort when tampering is detected" — that would
require an additional check (re-stat the source path and compare its
parent inode to the FD's inode) and would risk breaking legitimate
cases like the user moving a directory mid-session. Anchoring is
enough by itself to defeat the confidentiality / integrity attack;
we don't need an availability story on top.

## Platform notes

- `linkat` from an open FD requires `/proc/self/fd/<n>` (Linux). On
  macOS this path doesn't exist, so the linkat step returns
  `Unsupported` and `sync_file` falls through to the copy path. The
  copy path is also fully anchored on macOS via `openat` /
  `renameat`, which both platforms have.
- All `*at()` syscalls used here are present on both glibc and musl;
  no `#[cfg(target_env = "...")]` gating needed.
