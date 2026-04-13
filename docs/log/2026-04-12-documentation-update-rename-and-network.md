# Documentation update: rename to airlock and network rule changes

## Summary

Updated `README.md` and `docs/DESIGN.md` to reflect the binary rename from
`ezpez`/`ez` to `airlock`/`airlockd` and the network filtering rework.

## README.md changes

- Title: `# ezpez` → `# airlock`
- All `ez go`, `ez exec`, `ez x`, `ez help` commands → `airlock *`
- Install URL: `milankinen/ezpez` → `milankinen/airlock`
- Config file names: `ez.toml`/`ez.local.toml` → `airlock.toml`/`airlock.local.toml`
- Config load paths: `~/.ezpez/config.toml`/`~/.ez.toml` → `~/.cache/airlock/config.toml`/`~/.airlock.toml`
- Noted YAML/JSON support alongside TOML
- Network rules section: removed `http:` prefix example, updated "blocked by default"
  to "allowed by default", added `deny` list and `default_mode` examples
- License section: `ez` → `airlock` binary, `ezpez supervisor` → `airlockd`

## DESIGN.md changes

### Binary rename
- All `ez`/`ezpez` references → `airlock`/`airlockd` throughout
- Cache paths: `~/.ezpez/` → `~/.cache/airlock/`
- Config files: `~/.ez.toml`, `ez.toml`, `ez.local.toml` → airlock equivalents

### Crun removal
- Overview diagram: removed `crun (OCI runtime)` block, replaced with
  "Assemble overlayfs, chroot, exec container process" in `airlockd` block
- Container execution section: replaced "OCI runtime: crun" with "Process spawning"
  describing the direct fork+chroot+exec approach
- `airlock exec` section: removed `build_exec_command`/`crun exec` details,
  simplified to describe CliService RPC forwarding
- Removed OCI config generation section (config.json is gone; process config
  is carried in the `start` RPC call)
- Build pipeline: `Alpine + crun + supervisor` → `Alpine + airlockd supervisor`
- Initramfs description: removed `crun` mention

### Protocol update
- `start()`: removed `tlsPassthrough`, added new parameters:
  `cmd, args, env, cwd, uid, gid, nestedVirt, harden` (process config)
  and `imageId, dirs, files, caches` (mount config)
- Added `CliService` interface entry (Unix socket for `airlock exec`)
- `exec()`: updated signature to include `cwd, env`

### VM init sequence
- Step 2 (Read `mounts.json`): removed — mounts now come via the `start` RPC
- Renumbered remaining steps
- File mount paths: `/.ez/files_rw`/`/.ez/files_ro` → `/airlock/.files/rw`/`/airlock/.files/ro`

### File mounts
- Staging dirs: `overlay/files_rw/`/`overlay/files_ro/` → `overlay/files/rw/`/`overlay/files/ro/`
- Container symlink targets updated to match

### Network rules
- Section renamed: "Allow-list rules" → "Allow/deny rules"
- Pattern formats: removed `http:host` entry
- Added `deny` list semantics and `default_mode` to decision logic description
- TLS section: removed `http:` prefix and `tlsPassthrough` list references;
  clarified passthrough applies to all middleware-free connections

### Project cache layout
- Path: `~/.ezpez/projects/` → `~/.cache/airlock/projects/`
- Added `guest_cwd` file
- Removed `overlay/config.json` and `overlay/mounts.json` (replaced by RPC)
- Updated `overlay/files_rw/` → `overlay/files/rw/`, `files_ro/` → `files/ro/`
- Image cache path: `~/.ezpez/images/` → `~/.cache/airlock/images/`
- Locking: `ez go` → `airlock go`
