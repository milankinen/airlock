# Lift tokio LocalSet and error printing from cmd_* to main

## Motivation

`cmd_start::main` and `cmd_exec::main` each created their own
`tokio::task::LocalSet`, wrapped their run path in
`local.run_until(...)`, and called `cli::error!` on any returned
`anyhow::Error` before mapping it to exit code 1. That pattern was
duplicated across the two command entry points, with minor formatting
drift between them (`"Error: {e:?}"` vs `"{e:#}"`), and leaked the
async-runtime detail into every command handler.

## Change

Both `cmd_start::main` and `cmd_exec::main` now return
`anyhow::Result<i32>`. `main.rs` owns:

- A single `LocalSet::new().run_until(...)` around the whole command
  dispatch — required because capnp RPC clients are `!Send` and need
  `spawn_local`, which must run inside a `LocalSet`.
- A single `result.unwrap_or_else(|e| { cli::error!("Error: {e:?}"); 1 })`
  at the dispatch boundary, so any propagated anyhow error is printed
  with a uniform chain format.

Synchronous commands (`show`, `remove`, `secrets`) keep their `i32`
return and are wrapped in `Ok(...)` at the match-arm level.

## Why early-exit paths still return `Ok(exit_code)`

`cmd_start::main` has several early-exit paths that already print
their own context-specific error messages (missing cwd, failed to
create `.airlock/`, no `airlock.toml`, config load error, user
aborted the init prompt, etc.). These intentionally return
`Ok(1)`/`Ok(2)`/`Ok(0)` rather than propagating an `Err`, so the
unified handler in `main.rs` doesn't also print a generic
`"Error: ..."` on top of the specific message. Only anyhow errors
from the main `run()` path — which carry useful chained context —
flow up to the top-level printer.

## Files

- `app/airlock-cli/src/main.rs` — LocalSet + error printing.
- `app/airlock-cli/src/cli/cmd_start.rs` — signature change,
  remove local LocalSet, convert `return n;` → `return Ok(n);` on
  pre-`run()` paths.
- `app/airlock-cli/src/cli/cmd_exec.rs` — signature change,
  remove local LocalSet, drop inline error print.
