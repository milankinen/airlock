# File mounts via symlinks; init API consolidation; info logging

## File mount EACCES fix

After the 6b8f8d9 canonicalization commit switched file mounts from
overlayfs symlinks to per-file VirtioFS bind mounts, reading bind-mounted
files started failing with `Permission denied` even though `ls` showed
correct permissions (`rw-r--r-- root root`).

Root cause: Linux VirtioFS **directory** bind mounts work correctly, but
**file** bind mounts fail on `open()` despite `stat()` succeeding. The
`getattr` FUSE operation returns metadata without an access check, while
`open()` goes through VirtioFS's permission path which denies the request.
The confirmed workaround: never bind-mount individual VirtioFS files.

The fix reverts to the original symlink approach with a corrected
implementation:

1. `/mnt/overlay/files_rw` and `/mnt/overlay/files_ro` are **directory**
   bind-mounted into the container at `/ez/.files/rw` and `/ez/.files/ro`.
   VirtioFS directory bind mounts work fine.
2. For each file mount, a **symlink** is created at the target path pointing
   to `/ez/.files/{rw,ro}/<rel>`.

Reads follow the symlink → VirtioFS directory → host file (no EACCES).
Writes also propagate back through VirtioFS to the host file, keeping the
mounts live (unlike the staging/copy approach considered and discarded).
Stale entries (old anchor files, old symlinks with different targets) are
removed before creating the new symlink via `remove_file`.

The colleague's concern about symlinks being shadowed by dir bind mounts
(targets inside `guest_cwd`) still applies, but is not a real use case in
current configs.

## init API consolidation

`setup_container_mounts` was a separate public function called from
`main.rs` right after `setup()`. Merged it into `setup()` as a private
helper, extending `setup()`'s signature to accept `sockets` and
`nested_virt`. `main.rs` now makes a single `init::setup(...)` call.

## Info-level logging for user config

Mount, socket, cache, and disk log messages promoted from `debug!` to
`info!` so they appear at the default log level:

- `dir: <src> → <dst>` — directory bind mounts
- `file: <dst> → /ez/.files/{rw,ro}/...` — file mount symlinks
- `socket: <src> → <dst>` — socket forwards
- `cache: <name> → <dst>` — cache bind mounts
- `/ez/disk → ...` — disk/tmpfs selection

Default log level changed from `warn` to `info`. The `info` filter string
was also fixed: `ezpez_supervisor=trace` → `ezpez_supervisor=info` (trace
was unintentionally noisy).
