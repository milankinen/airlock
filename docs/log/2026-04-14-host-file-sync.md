# Host-side file-mount sync via notify + hard-link re-establishment

**Date:** 2026-04-14

## Problem

File mounts expose individual host files inside the guest via a hard-linked
staging directory (`overlay/files/rw/{key}`). When a guest application writes
atomically — the typical pattern: write to a temp file then `rename(tmp,
target)` — virtiofsd executes the rename on the host. This replaces the
directory entry in `overlay/files/rw/` with a new inode, severing the hard
link to the original source file. After that rename the source file on the
host is never updated, even though `overlay/files/rw/{key}` contains the new
content.

Confirmed by inspecting inodes: `overlay/files/rw/claude-json` had inode
68452197 (new, link count 1), while `~/.ez/claude.json` still had inode
67450709 (old, 922 bytes stale vs 21795 bytes in overlay).

## Solution overview

A host-side background task watches `overlay/files/rw/` with the OS-native
change-notification API (FSEvents on macOS via `fsevent-sys`, inotify on Linux)
using the `notify` crate (`RecommendedWatcher`). On each change event it syncs
the overlay file back to its source path using a three-step strategy:

1. **Same inode check** — if `overlay/{key}` and the source file share an
   inode, the hard link was never severed; the source already has the new
   content and there is nothing to do.

2. **Re-establish the hard link** atomically:
   ```
   hard_link(overlay/{key}, source_dir/.{name}.airlock_sync)
   rename(.{name}.airlock_sync, source)
   ```
   After this the source file *is* the overlay file. Future direct writes
   (non-atomic) flow back without any further sync event because the link is
   intact again.

3. **Fall back to `tokio::fs::copy`** — handles cross-device paths and
   permission edge cases where hard linking is not possible. Uses
   `spawn_blocking` internally so it does not block the single-threaded
   async runtime.

## `SyncHandle` design

The watcher and the tokio task are bundled in `SyncHandle`:

```rust
pub(super) struct SyncHandle {
    task: Option<JoinHandle<()>>,
    watcher: Option<RecommendedWatcher>,
}
```

Fields are wrapped in `Option` to allow both `Drop` (abort path) and
`shutdown()` (graceful path) to take ownership via `.take()`, which is
required because the type implements `Drop` and Rust disallows moves out of
types with destructors.

- **`Drop`** — aborts the task immediately. Used on error paths where
  `VmInstance` is dropped without calling `shutdown()`.
- **`shutdown()`** — drops the watcher first (this closes the `tx` end of the
  mpsc channel that lives in the watcher callback), then awaits the task so
  any buffered events are drained before the VM is killed.

## Graceful shutdown flow

`VmInstance::shutdown()` is an async fn that consumes ownership:

```
supervisor.shutdown().await    // flush guest filesystems
vm.shutdown().await            // drain sync events, then drop VM
```

Dropping the watcher causes `rx.recv()` inside `watch_loop` to return `None`
(channel closed) after all buffered events are processed. The task exits
naturally. Only then is the VM handle dropped, ensuring in-flight writes are
fully synced before the guest disappears.

## Why `notify` instead of polling

An earlier polling implementation scanned inode+mtime on a 100 ms timer.
`notify` eliminates the latency and CPU overhead: FSEvents delivers change
events within milliseconds and incurs no CPU cost when files are idle.

## Files changed

- `crates/airlock/src/vm/file_sync.rs` — new module: `SyncHandle`, `start()`,
  `watch_loop()`, `sync_file()`
- `crates/airlock/src/vm.rs` — add `sync_handle` field, `VmInstance::shutdown()`
- `crates/airlock/src/cli/cmd_up.rs` — `drop(vm)` → `vm.shutdown().await`
- `Cargo.toml` / `crates/airlock/Cargo.toml` — add `notify = "6"` dependency
