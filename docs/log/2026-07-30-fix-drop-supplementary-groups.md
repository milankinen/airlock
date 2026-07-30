# Drop root's supplementary groups before container setuid

## Symptom

A container process launched as an unprivileged user still carried airlockd's
(root's) supplementary group list — typically including GID 0 — so it could
read and write group-owned files it should not have been able to reach.

## Root cause

The pre-exec hook that drops privileges called `setgid` then `setuid` but
never `setgroups`. `setuid` drops the primary uid/gid but leaves the inherited
supplementary group list intact, so root's groups survived into the sandboxed
process.

## Fix

Call `setgroups(0, NULL)` in the pre-exec hook, before `setgid`/`setuid`
(while still root, since clearing groups requires privilege). This empties the
supplementary group list so the container process only has the primary
uid/gid it was assigned. `setgroups` is a bare syscall and therefore
async-signal-safe, which the post-fork/pre-exec context requires. A failure is
reported through the existing diagnostic pipe as `setgroups`.

## Testing

`cargo clippy -p airlockd` and `mise lint` are clean. The privilege-drop path
is exercised by the VM bats suite (`bats:vm`, needs KVM + Docker), not run
here.
