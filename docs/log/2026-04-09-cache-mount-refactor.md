# Cache mount refactor: named dirs, multi-path, enabled flag

**Date:** 2026-04-09

## Problem

The previous cache mount design used the container path as the key for the
on-disk directory (`/mnt/disk/cache/<rel-path>/`). This had two issues:

1. **Stale dirs**: if a cache entry was removed or renamed, the old disk
   directory was left behind permanently.
2. **Single path**: each cache entry could only cover one container path,
   making it awkward to cache related dirs (e.g. `~/.cargo/registry` and
   `~/.cargo/git` needed separate entries with separate disk overhead).

## Solution

### Named cache directories

Cache entries now use their BTreeMap key (config name) as the on-disk
directory name: `/mnt/disk/cache/<name>/`. Paths within that directory
mirror the container path structure (same as before, but scoped under
the named dir).

### Stale cleanup

`setup_disk` in the supervisor now reads `/mnt/disk/cache/`, compares
existing subdirectory names against the known set from `mounts.json`,
and deletes any that are no longer declared. This runs on every `ez go`
start, so orphaned dirs from removed/renamed entries are cleaned up
automatically.

### Multi-path (`paths: Vec<String>`)

`CacheMount.path: String` changed to `CacheMount.paths: Vec<String>`.
A single named cache can now back multiple container paths, all sharing
the same on-disk name directory. This is useful for grouping related
paths (e.g. `~/.rustup/toolchains` + `~/.rustup/update-hashes`).

### Enabled flag propagation

Previously `oci/cache.rs` filtered to only enabled entries before
returning. Now it returns ALL entries (enabled + disabled) with an
`enabled: bool` field. The supervisor uses this to:
- Create disk dirs for all declared names (even disabled ones — so
  re-enabling is cheap)
- Only bind-mount the enabled ones into the container rootfs
- Delete dirs for names not in the list at all (truly removed entries)

## Files changed

- `cli/src/config.rs` — `CacheMount.paths: Vec<String>`, updated docs
- `cli/src/oci/cache.rs` — `CacheEntry` type alias, returns all entries
- `cli/src/oci.rs` — `mounts.json` cache format: `{name, enabled, paths}`
- `sandbox/supervisor/src/init/linux.rs` — `CacheMount` struct, stale
  cleanup, named dir layout in `setup_disk`/`assemble_rootfs`
- `cli/src/vm.rs` — `flat_map` over `paths`
- `cli/src/cli/cmd_project_info.rs` — `paths.join(", ")`
- `cli/src/config/presets/{rust,nodejs,python}.toml` — `paths = [...]`
- `ez.toml` — updated cache entries to `paths = [...]`
