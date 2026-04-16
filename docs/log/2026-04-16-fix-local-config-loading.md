# Fix airlock.local.toml not being loaded

## Problem

Environment variables (and any other settings) defined in
`airlock.local.toml` were silently ignored. The file was never loaded.

## Root cause

`load_first()` in `load_config.rs` used `Path::with_extension()` to
append file extensions to base paths like `airlock.local`. Rust's
`Path::with_extension("toml")` treats `.local` as an existing extension
and *replaces* it, producing `airlock.toml` instead of the intended
`airlock.local.toml`. This caused `airlock.toml` to be loaded twice
and `airlock.local.toml` to be skipped entirely.

## Fix

Replaced `base.with_extension(ext)` with string formatting:
`format!("{}.{ext}", base.display())` which appends the extension
unconditionally.

## Debugging support

Added tracing instrumentation to config loading so future issues are
observable:

- `debug`: logs each config file found and each preset applied
- `trace`: logs parsed config values and final merged result

To make these logs observable during config loading, the logging setup
was moved earlier in the startup sequence — `.airlock/` directory and
log file are now initialized before config is loaded. The log file
moved from `.airlock/sandbox/run.log` to `.airlock/airlock.log`.
