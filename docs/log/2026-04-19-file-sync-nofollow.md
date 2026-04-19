# Fix file-sync symlink TOCTOU with O_NOFOLLOW + FD operations

The host-side file-sync task watches `overlay/files/rw/` and
propagates guest-visible writes back to the original host source
paths. The overlay directory is exposed to the guest over virtiofs,
so the guest is the writer — and an attacker in the guest can swap
the directory entry at any time.

The previous `sync_file` was a textbook check-then-use:

```rust
let overlay_ino = std::fs::metadata(overlay_path)?.ino();   // stat (follows)
std::fs::hard_link(overlay_path, &tmp)?;                    // path-based
std::fs::rename(&tmp, source)?;                             // replace source
```

If the overlay entry flips from a regular file (the expected inode
that was hard-linked to `source`) to a symlink pointing at, say,
`/etc/passwd` between the `metadata` call and the `hard_link` call,
the resulting `tmp` is a hard link to the symlink itself (Rust's
`hard_link` uses `link(2)` which does not dereference). The subsequent
`rename(tmp, source)` then installs a symlink to `/etc/passwd` at the
user's host source path. Next time anything reads that path it follows
the symlink to a file the user never intended to expose.

## Fix

`sync_file` now opens the overlay path **once** with `O_NOFOLLOW`, and
every subsequent operation targets the resulting FD rather than the
path:

1. `open_nofollow(overlay_path)` — `OpenOptions::custom_flags(O_NOFOLLOW)`.
   If the entry is a symlink at open time, `open` returns `ELOOP`
   and we log-and-skip.
2. `File::metadata()` is `fstat(2)` on the FD — the inode we check
   is the inode we hold.
3. Same-inode short-circuit is unchanged.
4. Hard-link re-establishment uses `linkat(AT_FDCWD, "/proc/self/fd/N",
   AT_FDCWD, tmp, AT_SYMLINK_FOLLOW)` on Linux. The proc magic link
   resolves to the FD's underlying inode, so the `linkat` creates a
   new link to exactly what we fstat'd regardless of what lives at
   `overlay_path` by the time the kernel runs the syscall.
5. The copy fallback reads from the FD (via `io::copy(&mut &file, …)`)
   into a `O_CREAT|O_EXCL`-mode-0600 tmp file in `source`'s directory
   and `rename`s into place — again never re-opening the overlay path.

The original `tokio::fs::copy` fallback was replaced with a synchronous
FD-based copy. The surrounding function was already doing synchronous
std::fs calls (`metadata`, `hard_link`, `rename`), so there's no new
blocking behavior — just a different syscall shape.

## Cross-platform

Linux has `/proc/self/fd` and `linkat` with `AT_SYMLINK_FOLLOW`, so
the hard-link optimization stays safe and cheap there. macOS has
neither (no `/proc`, and `linkat(AT_EMPTY_PATH)` isn't available).
`linkat_from_fd` returns `ErrorKind::Unsupported` on non-Linux and
`sync_file` falls through to the FD-based copy path. That's a small
regression for macOS developers editing large files inside rw
file-mounts — every atomic save becomes a copy rather than a re-linked
hard link — but macOS is still affected by the same underlying class
of attack so skipping the optimization is the right call.

## What's still path-based

Two spots intentionally remain path-based and are not TOCTOU-able in a
harmful way:

- `std::fs::metadata(source)` for the same-inode check. `source` is a
  host path chosen by the user at config time; it sits outside any
  directory the guest can write into. A stat race on `source` would
  only cause us to skip or do an unnecessary copy, not misdirect a
  write.
- `rename(tmp, source)`. `rename` replaces the directory entry at
  `source`; the destination directory is host-owned and the tmp file
  name is under our control. A guest cannot race this.

## read_state in the watch loop

`watch_loop` still calls `std::fs::metadata(files_rw_dir.join(key))`
to keep per-file inode/mtime baselines. That metadata call is only
used to decide whether to **dispatch** a sync and never to operate on
the file; the hardening lives in `sync_file` where the dangerous
operations (link, copy, rename) actually happen. Keeping `read_state`
path-based avoids a second FD open per event for no security benefit.
