# Rename project from ezpez to airlock

Full project rename. The old name "ezpez" was a placeholder; "airlock"
better captures the project's purpose — a sealed, controlled entry point
for untrusted code execution.

## What changed

Every occurrence of "ez"/"ezpez"/"ezd" across the codebase was replaced:

| Old | New |
|-----|-----|
| CLI binary `ez` | `airlock` |
| Daemon binary `ezd` | `airlockd` |
| Package `ezpez-cli` | `airlock-cli` |
| Package `ezpez-supervisor` | `airlock-supervisor` |
| Package `ezpez-protocol` | `airlock-protocol` |
| Cache dir `~/.ezpez/` | `~/.cache/airlock/` |
| Config file `ez.toml` | `airlock.toml` |
| Config file `ez.local.toml` | `airlock.local.toml` |
| User config `~/.ez.toml` | `~/.airlock.toml` |
| Env var `EZPEZ_ASSETS_CHECKSUM` | `AIRLOCK_ASSETS_CHECKSUM` |
| Kernel cmdline `ezpez.epoch/shares/host_ports` | `airlock.*` |
| TLS cert CN `"ezpez CA"` / `"ezpez {host}"` | `"airlock CA"` / `"airlock {host}"` |
| macOS keychain service `"ezpez-registry"` | `"airlock-registry"` |
| Dispatch queue `"com.ezpez.vm"` | `"com.airlock.vm"` |
| Disk label `"ezpez-disk"` | `"airlock-disk"` |
| Log target `"ez::ezd"` | `"airlock::airlockd"` |
| VM hostname `ezvm` | `airlock` |
| In-VM paths `/ez/disk`, `/ez/.files/` | `/airlock/disk`, `/airlock/.files/` |
| Docker images/volumes `ezpez-*` | `airlock-*` |
| GitHub repo `milankinen/ezpez` | `milankinen/airlock` |
| mise task `[tasks.ez]` | `[tasks.airlock]` |

## Directory renames

- `crates/ez/` → `crates/airlock/`
- `crates/ezd/` → `crates/airlockd/`
- `mise/tasks/build/ezd` → `mise/tasks/build/airlockd`
- `ez.toml` → `airlock.toml`
- `ez.local.toml` → `airlock.local.toml`
- `ezpez.iml` → `airlock.iml`

## Cache directory change

The old cache lived directly in `~/.ezpez/`. The new location is
`~/.cache/airlock/`, following the XDG convention of keeping application
caches under `~/.cache/`. Existing cached data (kernel, images, projects)
will not be migrated automatically — users will need to extract assets
fresh on first run.
