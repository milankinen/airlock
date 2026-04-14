# CLI refactor: rename commands, move Args to cmd files, add verbose + disk

## Summary

Renamed `up` → `start`, `down` → `rm`, `info` → `show`. Moved clap `Args`
structs into each command's own file. Added `--verbose` flag to `start` and
`show` with a `cli::verbose!` macro. Added disk utilization display (`show`
always, `start` with `--verbose`).

## Changes

### Command renames

- `airlock up` → `airlock start`
- `airlock down` → `airlock rm`
- `airlock info` → `airlock show`

The old names were inconsistent with common container-tool conventions (e.g.
`docker rm`). `start` / `rm` / `show` are clearer and more familiar.

### Newtype Command enum

The `Command` enum in `cli.rs` previously inlined all argument fields directly
as enum variant fields:

```rust
enum Command {
    Up { log_level: LogLevel, sandbox_cwd: Option<String>, login: bool },
    Down { force: bool },
    ...
}
```

Each cmd file now defines its own `#[derive(Args)] pub struct FooArgs` and the
`Command` enum wraps them as newtype variants:

```rust
enum Command {
    Start(cmd_start::StartArgs),
    Rm(cmd_rm::RmArgs),
    Exec(cmd_exec::ExecArgs),
    Show(cmd_show::ShowArgs),
}
```

This keeps argument declarations co-located with their handling code.

Note: `CliArgs` and `LogLevel` remain in `cli.rs` because they are used by
`vm.rs`, `oci.rs`, and `rpc/supervisor.rs` as runtime (non-clap) types.
`StartArgs` converts to `CliArgs` inside `cmd_start::run`.

### Verbose flag

`--verbose` / `-v` is available on `start` and `show`. Backed by a
`VERBOSE: AtomicBool` in `cli.rs`, set via `cli::set_verbose()` at the start
of each command's `run()`.

`cli::verbose!(...)` is a new macro that prints to stderr (like `cli::log!`)
only when verbose is set and not silent. Both flags compose: `--quiet` always
suppresses, `--verbose` adds detail on top of normal output.

Verbose output in `start`: enabled mounts and network rules printed during the
"Preparing sandbox..." phase, plus disk utilization after the VM boots.

Verbose output in `show`: mounts and network rules sections are gated behind
`--verbose`. Disk utilization is always shown when a disk image exists.

### Disk utilization

Disk images are sparse files — `metadata().len()` is the apparent/virtual size
while `metadata().blocks() * 512` is actual allocated space. `show` now prints
`Disk: <used> / <total>` after the sandbox path. `start --verbose` prints the
same after the disk is prepared.

`cli::format_bytes(u64) -> String` is a new shared helper formatting bytes as
`x.y GB` / `x.y MB` / `n KB`.

## Files changed

- `crates/airlock/src/cli.rs` — updated `Command` enum; added `VERBOSE`,
  `is_verbose()`, `set_verbose()`, `verbose!` macro, `format_bytes()`
- `crates/airlock/src/cli/cmd_start.rs` (renamed from `cmd_up.rs`) — owns
  `StartArgs`; calls `set_verbose`; verbose mounts/network/disk output
- `crates/airlock/src/cli/cmd_rm.rs` (renamed from `cmd_down.rs`) — owns `RmArgs`
- `crates/airlock/src/cli/cmd_show.rs` (renamed from `cmd_info.rs`) — owns
  `ShowArgs`; verbose flag gates mounts/network; always shows disk utilization
- `crates/airlock/src/cli/cmd_exec.rs` — owns `ExecArgs`; `run` takes `ExecArgs`
- `crates/airlock/src/main.rs` — updated dispatch to newtype match arms
