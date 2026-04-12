# Rename ezpez → airlock and CLI restructure

## Summary

Full project rename from "ezpez" to "airlock" combined with a CLI
restructure to simplify the command surface.

## Rename

Every user-visible identifier was changed:

| Old | New |
|-----|-----|
| `ez` (binary) | `airlock` |
| `ezd` (daemon) | `airlockd` |
| `crates/ez` | `crates/airlock` |
| `crates/ezd` | `crates/airlockd` |
| `ezpez-cli` | `airlock-cli` |
| `ezpez-supervisor` | `airlock-supervisor` |
| `ezpez-protocol` | `airlock-protocol` |
| `~/.ezpez/` | `~/.cache/airlock/` |
| `ez.toml` | `airlock.toml` |
| `ez.local.toml` | `airlock.local.toml` |
| `~/.ez.toml` | `~/.airlock.toml` |
| `EZPEZ_*` env vars | `AIRLOCK_*` |

String literals throughout the Rust source were updated: TLS cert CN
(`"airlock CA"`), macOS dispatch queue (`"com.airlock.vm"`), keychain
service name (`"airlock-registry"`), kernel cmdline params
(`airlock.epoch`, `airlock.shares`, `airlock.host_ports`), disk label
(`"airlock-disk"`), log targets (`airlock::airlockd`).

The `~/.ez/claude.json` path referenced in `airlock.toml` was kept
as-is since it is the user's personal host configuration, not project
branding.

## CLI restructure

### `airlock go` → `airlock up`

The `go` subcommand was renamed to `up`. Non-TTY invocations now print
an error and exit with code 2 immediately rather than attempting to
start a VM with no terminal.

### Sessions removed

Session support (`--session` flag, `<hash>.<session>` cache directory
naming) was removed entirely. Sessions added complexity without
sufficient benefit — each project directory now maps 1:1 with its
host path hash.

### Flat project subcommands

The `project` namespace was eliminated:

| Old | New |
|-----|-----|
| `airlock project list` | `airlock list` |
| `airlock project info [path]` | `airlock info [path]` |
| `airlock project remove [path]` | `airlock down [path]` |

The `down` command uses `dialoguer::Select` with `ColorfulTheme` for
its interactive confirmation prompt, replacing the previous plain text
confirmation. The `--yes`/`-y` flag was renamed to `--force`/`-f`.
Items are ordered `[Yes / No]` with `No` as the default (safe choice).

### Distroless version format

The `--version` output now distinguishes distroless builds:

```
1.0.0 [distroless] (abc1234)   # distroless build
1.0.0 (abc1234)                 # standard build
```

### `airlock up [path]` — optional project directory

`airlock up` now accepts an optional positional path argument (like
`airlock down`). When omitted it defaults to the current directory;
when given it must be an existing directory.

If the resolved directory contains no `airlock.toml`, `airlock.json`,
`airlock.yaml`, or `airlock.yml` (nor their `.local.*` variants), the
user is prompted with a `Select` to initialize or cancel. Choosing
"Initialize with defaults" writes a minimal `airlock.toml`:

```toml
[vm]
# image = "alpine:latest"
```

`project::lock()` was refactored to accept an explicit `PathBuf`
instead of calling `current_dir()` internally — the caller is now
responsible for resolving the project directory, which avoids the
redundant cwd lookup that was already happening in `cmd_up`.

### JSON and YAML config reading

Config loading now supports `.json`, `.yaml`, and `.yml` in addition to
`.toml` for all four config slots (global, home, project, local).
For each slot the first matching extension is used (`toml` → `json` →
`yaml` → `yml`). `serde_yaml` (v0.9) was added as a workspace
dependency. The `has_config` check in `cmd_up` was updated to match.

## Design notes

The `parse_id()` helper (which split `<hash>.<session>` directory
names) was removed since all project cache directories are now plain
32-character hex hashes. `min_unique_prefix_len` was simplified
accordingly — no deduplication by hash-part needed, just operate
directly on the full ID list.

`dialoguer::Select` is used throughout for interactive prompts
(instead of `Confirm`) for visual consistency:
- `airlock down`: `[Yes / No]`, default `No`
- `airlock up` init: `[Initialize with defaults / Cancel]`, default `Initialize`
- Registry credential prompts use `ColorfulTheme` with `Input`/`Password`
