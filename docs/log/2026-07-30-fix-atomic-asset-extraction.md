# Fix atomic VM asset extraction and lock concurrent first-runs

## Symptom

Two `airlock` processes started at nearly the same moment right after an
upgrade could corrupt the VM boot files. Both would notice the bundled asset
checksum had changed and both would start rewriting `~/.cache/airlock/vm/Image`
(the kernel) and `initramfs.gz` at the same time. One process could then try to
boot cloud-hypervisor from a kernel image the other process had just truncated
to zero bytes and not finished rewriting, producing a failed or garbled boot.

## Root cause

Asset extraction had two gaps:

1. The kernel and initramfs were written **in place** with a truncating
   `std::fs::write`. That opens the destination, empties it, and streams the new
   bytes — so for a moment the file on disk is short or empty. Anything reading
   it during that window sees a broken file. (The bundled executables already
   avoided this by writing to a temp file and renaming it into place; the two
   boot files did not.)

2. There was **no lock** around the "has the checksum changed? if so, extract"
   sequence. Nothing stopped two processes from running that sequence at the
   same time, so both could extract concurrently and interleave their writes.

## Fix

- All extracted assets are now written the same safe way: bytes go to a
  temporary file first, then the temp file is atomically renamed over the final
  name. A reader always sees either the complete old file or the complete new
  one, never a half-written one. A new `write_atomic` helper does this for the
  kernel and initramfs (the executables keep their existing `write_executable`
  helper, which additionally sets the executable bit).

- The whole check-and-extract sequence is now serialized across processes with
  an exclusive file lock (`flock`) on a sidecar `lock` file in the VM cache
  directory. The first process in takes the lock and extracts; a second process
  waits, and by the time it gets the lock the checksum has already been updated,
  so it simply skips extraction. This reuses the same locking pattern already
  used for the sandbox lock and the secret vault.

## Tests

- Added a unit test for `write_atomic`: it writes a file, overwrites it with
  different contents, and confirms the final contents are correct and no
  leftover temp file remains.

- The cross-process race itself is not unit-tested: the lock is blocking (a
  single-threaded test taking it twice would just hang) and the real extract
  path embeds the multi-megabyte VM images and is excluded from test builds.
  The underlying `flock` behavior is already covered by the sandbox-lock test in
  `project.rs`, and the new lock helper mirrors that proven pattern.
