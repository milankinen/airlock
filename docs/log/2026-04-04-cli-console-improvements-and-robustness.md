# CLI console improvements and robustness

### Console module (`cli.rs`)

Merged `console.rs` into `cli.rs`. Renamed `Cli` struct to `CliArgs`
to avoid confusion with the module name. All console utilities now
accessed as `cli::log!`, `cli::check()`, `cli::dim()`, etc.

Added:

- `cli::error!` macro for red error output
- `cli::check()` green checkmark, `cli::bullet()` for detail lines
- `cli::dim()` grey text for secondary values (digests, sizes, etc.)
- `cli::red()` for error messages
- `cli::interrupted()` / `cli::is_interrupted()` via `watch` channel
  (race-free signal handling for Ctrl+C during downloads)
- `cli::is_interactive()` for interactive prompts
- Progress bars with byte-level updates via `ProgressWriter`
- Image change prompt (re-create/continue/cancel) via dialoguer

### Download robustness

- Raw terminal mode deferred until VM boot — Ctrl+C works during
  downloads
- Downloads write to `.tmp` files, renamed atomically on success
- Layer size verified after download, corrupt files re-downloaded
- `.tmp` cleanup on startup for interrupted downloads
- `tokio::select!` races each download against `interrupted()`

### Bundle consistency

Digest file is the atomicity marker — written last after successful
rootfs copy. Missing = incomplete (clean up and recreate). Mismatched
= image changed (prompt user).

### Project locking

PID-based lockfile prevents concurrent instances from modifying the
same project. Stale locks (dead PID) are taken over. Atomic via
write-to-tmp + rename + verify pattern. Released on drop.

### Error handling

Removed `CliError` enum — all errors use `anyhow::Result` now.
Top-level errors printed in red.

### Logging

Replaced `verbose :Bool` with `logFilter :Text` in RPC schema.
Both CLI and supervisor use the same `EnvFilter` string. Configurable
via `--log-level` flag (trace/debug/info/warn/error).
