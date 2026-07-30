# Reap orphaned zombies in the guest PID 1

## Symptom

airlockd runs as PID 1 inside the guest. Any process whose parent exits — a
double-forking daemon (`sh -c 'daemon &'` then exit), or anything that
background-and-detaches — is reparented to PID 1. airlockd never reaped those
orphans, so they accumulated as `<defunct>` zombies until the guest ran out of
PIDs and could no longer fork.

## Root cause

tokio only reaps the child processes it spawned (it `waitpid`s the specific
PIDs it knows). Reparented orphans are not among them, and nothing else called
`wait`, so they lingered. The obvious fix — a `waitpid(-1, WNOHANG)` reaper —
is unsafe here: it would race tokio and consume the exit status of a process
airlockd spawned and is `Child::wait`-ing on, breaking exit-code reporting for
every user command and daemon.

## Fix

Added a periodic orphan reaper (`run_orphan_reaper`, started from `airlockd()`
before anything spawns). Every 2 seconds it scans `/proc` for zombie
(state `Z`) processes whose parent is this PID and reaps each one individually
with `waitpid(pid, WNOHANG)` — but skips any PID airlockd spawned itself.

Those PIDs are tracked in a small registry (`own_children`), populated wherever
airlockd spawns via tokio (`spawn` for pty/pipe processes and `spawn_daemon`).
Enumerating through `/proc` rather than `waitpid(-1)` is what makes the
exclusion possible: we target the exact orphan PIDs instead of blindly
consuming whatever zombie comes first, so tokio's `Child::wait` still sees its
own children's exits.

The registry is self-pruning: each pass drops entries whose PID no longer
appears in `/proc` (tokio already reaped it), which bounds the set and lets a
reused PID re-register cleanly. A tokio child that is briefly a zombie before
tokio reaps it is protected because it is still in the registry during that
window.

## Tests

`process::tests::parse_stat_*` cover the `/proc/<pid>/stat` parser, including a
`comm` field containing spaces and parentheses (the parse keys off the last
`)`), which is the easy thing to get wrong. The reaper's `/proc` scanning and
its interaction with tokio are covered by the VM bats suite (`bats:vm`, needs
KVM + Docker), not run here.
