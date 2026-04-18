# `AIRLOCK_PTY_DUMP` — capture guest PTY output for offline replay

## Motivation

Diagnosing the HVP-vs-CUP rendering bug (previous commit) required
capturing the exact byte stream btop emitted inside the guest and
replaying it through `vt100` on the host. Doing that by hand once is
fine; building an env-gated dump facility makes the same diagnosis
trivial the next time some escape-sequence-heavy app breaks in the TUI.

## Design

When `AIRLOCK_PTY_DUMP=1` is set, `airlock start` opens
`<sandbox_dir>/pty.dump` (truncating) and appends every byte of guest
stdout/stderr to it, in both TUI (`--monitor`) and non-TUI modes. The
hook sits at the output boundary in `cmd_start.rs` — one call site per
process-event arm — so the TUI sees the same bytes whether or not the
dump is active. When the env var is unset the `Option<File>` is `None`
and the write helper is a no-op, so the non-dumping path pays only a
branch per output chunk.

`<sandbox_dir>` is the project's `.airlock/sandbox` directory, already
created for RPC sockets and meta files, so no extra directory handling
is needed. Path is logged on startup.

## Replay

`crates/tui/examples/vt100_replay.rs` reads the dump, feeds it through
`TuiTerminalSink` (same HVP rewriter + vt100 parser the TUI uses), and
prints the resulting grid row-by-row. Auto-sizes to the current
terminal (minus the tab bar) by default; explicit `rows cols` args
override. Running it against a captured dump reproduces exactly what
the TUI body rendered.

## Scope notes

Keeping the dump bytes-only (not timestamped, not framed) so the replay
tool can just `process()` the whole file. If we later need timing
information for animation bugs we can either ndjson-frame it or ship a
separate timed dump; for the current class of issues (positioning,
color, wide-char) a flat byte stream is sufficient.
