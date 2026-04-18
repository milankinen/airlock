# Live policy editing + quit-from-monitor

## What

Two user-facing additions to the `--monitor` TUI, plus plumbing to
support them:

1. **Policy dropdown on the Monitor tab's network panel.** The title
   bar now shows `policy: <Title> ▾`; pressing `p` or clicking the
   label opens a dropdown with all four policies. Up/Down highlights,
   Enter applies, Esc closes. The chosen policy takes effect on the
   next connection the sandbox makes — no restart.
2. **`q` / Ctrl+D on the Monitor tab asks the sandbox process to
   exit.** SIGHUP is sent first (interactive bash exits on HUP but
   ignores SIGINT/SIGTERM at an idle prompt), followed by SIGTERM as a
   fallback. When the guest process exits, the existing
   `TuiEvent::Exit` path tears the TUI down, so the user drops back to
   their host shell with no trailing sandbox.

## Why

The previous design printed the policy as a frozen string baked into
the title at startup — it couldn't be changed without restarting the
sandbox. A policy that's only readable from a config file defeats the
"dashboard" framing of the monitor tab. The dropdown makes the
`--monitor` tab actually interactive: you see what's allowed/denied,
and you can immediately widen or tighten the policy without exiting.

Quitting the sandbox *from* the monitor tab also used to be
impossible — the only way was F1 back to the sandbox terminal and a
manual `exit` / Ctrl+D into the guest shell. That's awkward when the
reason you're in the monitor tab is that something misbehaved and you
want out. `q` in a TUI context universally means quit.

## How: live policy

`Network` previously stored `policy: Policy` directly on a struct
full of `Rc`s (TLS interceptor, compiled Lua middleware). Those `Rc`s
make `Network` `!Send`, so the TUI thread (which is a plain
`std::thread`) can't hold a reference to it.

The minimal change: extract just the mutable bits into a separate
`NetworkState` struct and share it via `Arc<parking_lot::RwLock<_>>`.
Reads on the hot connect path are a single uncontended `read()`
call — `parking_lot::RwLock` has no poisoning and no atomic-ordering
surprises compared to `std::sync::RwLock`. `Network` keeps its
`!Send` bits private; it exposes `network.control()` which returns a
`NetworkControl` — a `Send + Sync` handle that only carries the
`Arc<RwLock<NetworkState>>`.

The TUI crate defines its own mirror `Policy` enum and a
`NetworkControl` trait; the airlock-side `NetworkControl` implements
the TUI trait via `From` conversions in both directions. This keeps
the `airlock-tui` crate a leaf — it doesn't depend on the airlock
crate or its `Policy` type.

## How: quit-from-monitor

The existing `Runtime::signals()` produced one stream of host OS
signals (SIGWINCH, SIGINT) to forward to the guest. The TUI needs to
inject its own signals into that same stream so the existing forwarder
picks them up without duplicating the plumbing.

- Changed `Runtime::signals(&self)` → `signals(&mut self)` so the
  monitor variant can take its channel receiver.
- `MonitorRuntime` now creates an `mpsc::channel::<i32>` in `new()`.
  In `signals()` it merges the OS stream and the TUI channel via
  `async_stream::stream!` + `tokio::select!`; in `launch()` it hands
  the sender to the TUI thread.
- In the TUI key handler, pressing `q` (no modifier) or Ctrl+D on the
  Monitor tab pushes SIGHUP(1) and SIGTERM(15) onto that channel.
- When the guest process exits, the supervisor's existing exit event
  travels back through `TuiEvent::Exit`, causing `run_tui_loop` to
  return — the TUI shuts down from *any* active tab, not just the
  Monitor tab.

### Why SIGHUP + SIGTERM (and not SIGINT)

- Interactive bash at an idle prompt ignores SIGINT. Pressing `q`
  would only work if bash was blocked on a command.
- Bash also ignores SIGTERM at an interactive prompt, by default.
- SIGHUP is the canonical "controlling terminal went away" signal.
  Interactive shells exit on SIGHUP, running any EXIT traps first.
  This matches the mental model — the user is closing the sandbox's
  "terminal" (the TUI), so HUP is precisely what would normally be
  sent when a terminal emulator window closes.
- SIGTERM follows as a backstop for non-shell processes that
  deliberately ignore HUP (daemons, tmux, …).

## Files touched

- `crates/airlock/src/network.rs` — split `NetworkState` out of
  `Network`, wrap in `Arc<RwLock<_>>`, add `policy()`/`control()`.
- `crates/airlock/src/network/control.rs` — new `NetworkControl`
  thread-safe handle + `From` impls to the airlock-tui enum.
- `crates/tui/src/network_control.rs` — TUI-side `Policy` enum,
  `NetworkControl` trait, `title()` / `color()` / `label()`.
- `crates/tui/src/tabs/monitor/network/{mod,chrome}.rs` — dropdown
  state + rendering, policy anchor click rect, keyboard + mouse
  handling.
- `crates/airlock/src/runtime.rs` + `runtime/{monitor,raw}_terminal.rs`
  — `signals(&mut self)`, TUI signal channel merged in the monitor
  runtime, `signals must be called before launch` assertion.
- `crates/tui/src/lib.rs` — thread `sig_tx` through the TUI, handle
  `q` / Ctrl+D on the Monitor tab, arc-dyn `NetworkControl` on `App`.
- `docs/manual/src/monitor.md` — document the dropdown and the
  `q` / Ctrl+D shortcut.
