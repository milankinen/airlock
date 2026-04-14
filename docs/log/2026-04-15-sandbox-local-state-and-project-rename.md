# Sandbox local state, project rename, and init hardening

## Summary

Replaced the global `~/.cache/airlock/projects/{hash}/` sandbox registry with
per-project local state at `<project-root>/.airlock/`. Renamed the central
`Sandbox` struct and module to `Project`. Replaced `.ref`/`.complete` markers
with a `meta.json` hard-link for image GC and download completion tracking.
Added an `.airlock/` bind-mount mask in the supervisor to hide sandbox
internals from the container. Hardened `init/linux.rs` so all I/O errors abort
initialization instead of being silently logged.

## Motivation

The previous design used a central registry keyed by a SHA-256 hash of the
project directory path. This had several problems:

- **Discoverability**: sandbox state was scattered in `~/.cache` with no
  obvious connection to the project tree. Finding which cache directories
  correspond to which projects required `airlock list` plus the opaque hash.
- **Portability**: the hash came from the absolute host path, so moving or
  renaming a project directory orphaned its sandbox state and created a new one.
- **Cleanup**: after `airlock down`, old sandbox entries in `~/.cache` could
  linger indefinitely. With local state, `airlock down` removes `.airlock/` in
  one step.

Silent errors during VM init were also a problem: a failed sysctl or iptables
rule would only appear in logs, and the VM would start in a broken state rather
than aborting cleanly with a clear error message.

## Design decisions

### Directory structure

```
<project>/
  .airlock/                  # cache_dir — removed entirely by `airlock down`
    .gitignore               # contains "*" — all sandbox state excluded from VCS
    sandbox/                 # sandbox_dir — CA, overlay, disk, lock, metadata
      ca.json                # CA cert + key PEM (JSON)
      ca/                    # overlayfs lowerdir for CA cert injection
      run.json               # last_run timestamp + guest_cwd
      overlay/               # overlayfs upper/work dirs + filelinks
      disk.img               # virtio-blk cache volume
      lock                   # PID lockfile (prevents concurrent airlock up)
      image                  # hard-link to ~/.cache/airlock/images/{digest}/meta.json
```

The `~/.cache/airlock/` directory is retained for:
- OCI image rootfs tarballs/unpacked layers (large, shared across sandboxes)
- Kernel + initramfs assets (build artifacts, shared)

### Two-level directory: cache_dir vs sandbox_dir

`Project` exposes two path fields:

- `cache_dir` = `<project>/.airlock/` — the outer wrapper; holds `.gitignore`
  and `sandbox/`. `airlock down` removes this entire directory.
- `sandbox_dir` = `<project>/.airlock/sandbox/` — all runtime state. The
  socket path, lock, CA, overlay, and metadata all live here.

`ensure_cache_dir()` creates `.airlock/` and writes the `*` gitignore on first
use. `lock()` then creates `sandbox/` inside it.

### meta.json replaces .ref and .complete

Previously two separate sentinel files tracked image lifecycle:

- `.complete` — written at end of download, checked before use to skip
  re-download
- `.ref` — hard-linked into `sandbox/image` to track which sandboxes reference
  an image for GC

Both are replaced by a single `meta.json` in the image cache directory:

```json
{ "digest": "sha256:...", "name": "docker.io/library/alpine:latest" }
```

- Written atomically at the end of image download (replaces `.complete`).
- Hard-linked into `sandbox/image` to serve as both GC ref and stored-digest
  source (replaces `.ref` + `run.json` image fields).
- GC checks `nlink()` on `meta.json`: if it is 1 (only the original, no sandbox
  hard-links), the image is unused and can be deleted.
- `read_sandbox_image_meta()` reads `sandbox/image` directly to get the stored
  digest without a separate field in `run.json`.

Migration: if `meta.json` is absent but `.complete` exists, it is written on
first `airlock up` without re-downloading.

### run.json reduced to last_run + guest_cwd

Previously `run.json` stored image name and digest. These are now read from
`sandbox/image` (the `meta.json` hard-link). `run.json` retains only:

- `last_run` — Unix timestamp of last `airlock up`, shown by `airlock info`
- `guest_cwd` — working directory inside the container (default: host cwd),
  read by `airlock exec`

### CA cert in memory

`generate_ca` writes a single `ca.json`:

```json
{ "cert": "-----BEGIN CERTIFICATE-----\n...", "key": "-----BEGIN PRIVATE KEY-----\n..." }
```

`Project::load()` and `Project::lock()` read this once into `ca_cert` and
`ca_key` `String` fields. All downstream consumers receive the PEM directly
from the `Project` struct, avoiding repeated file reads.

### .airlock/ mask in supervisor

The project directory is bind-mounted into the container (VirtioFS tag
`"project"`). The `.airlock/` directory inside it contains sensitive
runtime state (CA private key, disk image, overlay, PID lock). To prevent
accidental corruption by the container user, the supervisor bind-mounts an
empty read-only directory over `<rootfs>/<project-target>/.airlock/` as the
last mount in `assemble_rootfs`. The source directory is `/mnt/disk/mask` (on
disk) or `/tmp/airlock-mask` (tmpfs fallback when no disk is present).

### Init error propagation

All I/O and command-execution errors in `init/linux.rs` now propagate as
`anyhow::Result` instead of being silently swallowed with `warn!`/`error!`
log calls:

- `write_sysctl` returns `Result`, sysctl write failures abort init.
- `run_quiet` renamed to `run_cmd`, returns `Result`; non-zero exit codes
  and exec failures both bail.
- `setup_networking` returns `Result`; all `ip`/`iptables` calls propagate.
- `assemble_rootfs` propagates directory creation, symlink, and bind-mount
  errors instead of logging and continuing.
- `setup_disk` propagates `read_dir` and `remove_dir_all` errors during stale
  cache cleanup.
- `setup_container_mounts` propagates socket placeholder file creation errors.

### Drop of list command and path arguments

`airlock list` required the central registry; without it the command has no
data source. The command is removed.

`up`, `info`, and `down` previously accepted an optional path argument to
target a different project directory. With local state the invariant is
simpler: always run from the project directory. The arguments are removed;
the commands always resolve from `$PWD`.

## Files changed

- `crates/airlock/src/project.rs` — `Project` struct with `cache_dir` +
  `sandbox_dir`; `load()` and `lock()` resolve local state; replaces the
  old hash-based registry
- `crates/airlock/src/cache.rs` — removed `project_dir()`, updated docs
- `crates/airlock/src/oci.rs` — `meta.json` hard-link, `ImageMeta` struct,
  updated GC, migration from `.complete`
- `crates/airlock/src/cli.rs` — removed `List` variant, dropped path args
- `crates/airlock/src/cli/cmd_up.rs` — `project::lock()`, local log path
- `crates/airlock/src/cli/cmd_down.rs` — removes `cache_dir` (entire `.airlock/`)
- `crates/airlock/src/cli/cmd_info.rs` — shows `sandbox_dir`
- `crates/airlock/src/cli/cmd_exec.rs` — `project::load()`
- `crates/airlock/src/cli/cmd_list.rs` (deleted)
- `crates/airlock/src/assets.rs` — takes `&Project`
- `crates/airlock/src/network.rs` — reads CA from `project` struct fields
- `crates/airlock/src/rpc/supervisor.rs` — takes `&Project`
- `crates/airlock/src/vm.rs` — uses `project.sandbox_dir`
- `crates/airlockd/src/init/linux.rs` — `.airlock/` mask mount; full error
  propagation (`run_cmd`, `write_sysctl`, `setup_networking` all return `Result`)
- `docs/DESIGN.md` — updated project management section
- `Cargo.toml` — removed `sha2` dependency (no more project hash)
