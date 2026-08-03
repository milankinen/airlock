# Bound guest shutdown and honor Ctrl+C during boot

## Symptoms

Two related lifecycle problems in `airlock start`:

1. **Shutdown could hang forever.** After the main process exited, the CLI
   asked the guest to stop its daemons and flush filesystems and then awaited
   those RPCs with no timeout. If the guest supervisor was wedged (e.g. stuck
   mid-sync), `airlock start` hung after "process exited". Ctrl+C and SIGTERM
   did nothing useful at that point, so the user resorted to SIGKILL — which
   skips the VM's `Drop` and orphans the cloud-hypervisor / virtiofsd
   processes and leaves a stale lock file.

2. **First Ctrl+C during boot was lost.** Interruption was checked once, before
   `vm::start`. A SIGINT arriving during boot (the vsock connect retries for
   ~12s) was recorded in the interrupt flag but never consulted again, so the
   VM finished booting and dropped the user into an interactive session they
   had already cancelled.

## Fixes

1. The daemon-stop + filesystem-sync RPCs now run under a bounded
   `tokio::time::timeout` (`SHUTDOWN_TIMEOUT`, 30s). A healthy guest finishes
   well within it; a wedged guest hits the timeout, we log it, and the VM is
   torn down cleanly through its normal path regardless — no SIGKILL, no
   orphans. The timeout is intentionally generous so a legitimately slow sync
   isn't cut off (and the interrupt flag is deliberately *not* raced here,
   because a normal Ctrl+C exit already has it set, which would otherwise skip
   the sync entirely).

2. The interrupt flag is re-checked immediately after `vm::start` (before
   entering raw mode and building the supervisor session). If the user
   cancelled during boot, the freshly-booted VM is shut down and the command
   returns 130 (128 + SIGINT) instead of starting the session.

## Tests

No unit test: both changes are in the VM-lifecycle orchestration of
`run_start`, which requires a booted VM and a live supervisor. The behavior is
exercised by the VM bats suite (`bats:vm`, needs KVM + Docker), not run here.
