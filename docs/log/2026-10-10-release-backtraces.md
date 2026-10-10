# Hide backtraces in release builds

## Problem

A user with `RUST_BACKTRACE` set in the shell got a full backtrace from
the release binary on each panic. The final error print in `main`
(`{e:?}` on `anyhow::Error`) also prints a backtrace if
`RUST_LIB_BACKTRACE` or `RUST_BACKTRACE` is set. The output is noise for
users, and no Cargo profile setting turns it off. `strip` and
`debug = false` only remove the symbols, and the frames still print.

## Change

- `diagnostics::disable_release_backtraces`: in release builds
  (`not(debug_assertions)`), removes `RUST_BACKTRACE` and
  `RUST_LIB_BACKTRACE` from the process environment. In debug builds it
  does nothing.
- `main` calls it first, before the panic hook and before any other
  init step.
- std and anyhow read the variables on first use and then keep the
  value, so an early removal covers all later panics and errors.
- `remove_var` is `unsafe` in edition 2024. It is safe here because
  `main` uses the current-thread tokio runtime. That runtime has no
  worker threads, and its blocking pool starts threads only on first use.
- Side effect: child processes (VM helpers) also do not get the
  variables in release builds.

## Alternatives

- Print the panic without the default hook in release, and print errors
  with `{e:#}` instead of `{e:?}`. This leaves the environment alone, but
  it has two places to keep in sync, and other `{:?}` prints of errors
  still show backtraces.
- `std::panic::set_backtrace_style` is unstable.

## Tests

None. The change only removes two environment variables at start.
`cargo clippy --release` passes.
