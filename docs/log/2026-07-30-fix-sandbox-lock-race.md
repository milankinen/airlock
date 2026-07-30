# Fix sandbox lock allowing two concurrent runs

## Symptom

Two `airlock start` (or `up`) runs launched at nearly the same moment in one
project could both decide they held the sandbox lock, then boot two VMs
against the same `disk.img`, overlay, and sockets — corrupting sandbox state.

## Root cause

`acquire_lock` implemented locking by hand: read the lock file, check the
stored PID is alive, write our PID to a temp file, `rename` it over `lock`,
then read it back to "verify". `rename` clobbers unconditionally, so the
check-then-take sequence was a TOCTOU race — both racers saw no live holder,
both renamed, and each read back its own PID and returned success. The stale
check also used `kill(pid, 0)`, which reports a live process owned by another
user as absent (`EPERM`), so a foreign lock could be judged stale.

## Fix

Replaced the PID-file dance with a real kernel mutex: a non-blocking exclusive
`flock(LOCK_EX | LOCK_NB)` on `sandbox/lock`, held via a `File` kept in the
`Project` for its whole lifetime. Two racers can no longer both acquire — the
second `flock` returns `EWOULDBLOCK` and we report the holding PID (read from
the file, which we still write for diagnostics).

The lock releases automatically when the handle drops, and — importantly — the
kernel also releases it when the process exits even though `main` calls
`std::process::exit` (which skips destructors). This removes the old need to
unlink the lock file on drop, and with it the read-compare-remove TOCTOU that
`Drop for Project` had.

`is_running` now probes the same way (try the `flock`, release immediately if
acquired) instead of `kill(pid, 0)`, so it is immune to PID reuse and the
`EPERM` foreign-process case.

## Tests

`project::tests::lock_is_exclusive_and_is_running_tracks_it` — acquires the
lock, asserts a second acquisition is refused (even from the same process,
since `flock` treats independent fds as distinct holders) and that
`is_running` flips true while held and false after release.
