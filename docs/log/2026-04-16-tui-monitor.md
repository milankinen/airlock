# TUI monitoring control panel (`airlock start --monitor`)

## Motivation

The non-TUI `airlock start` path pipes stdin/stdout transparently between
the host terminal and the guest PTY. That works well for interactive use,
but offers no way to observe *what the sandbox is doing* — e.g., which
outbound connections are being made, which are being denied by policy.

This change introduces an optional `--monitor` mode: a ratatui-based
tabbed control panel that wraps the embedded sandbox terminal alongside
a live network connection log.

## Architecture

A new `airlock-tui` crate isolates ratatui/vt100/crossterm away from the
core CLI, so non-monitor users pay no compile-time cost for ratatui
machinery.

The TUI runs on a dedicated `std::thread` — fully decoupled from the
async Cap'n Proto RPC loop. Communication happens via channels:

- **To TUI:** process output (`Vec<u8>`), network events, exit code — all
  over `std::sync::mpsc`
- **From TUI:** keystrokes and resize events over `tokio::sync::mpsc`,
  consumed by a channel-backed `stdin::Server` RPC capability
  (`TuiStdin`), replacing the real `tokio::io::stdin()` on this path

This avoids the failure mode where a slow render would block the RPC
event loop, and the reverse (a busy RPC loop stalling the TUI).

The process output is fed into a `vt100::Parser` which maintains a cell
grid; the sandbox tab renders that grid into the ratatui buffer each
frame (`~60fps`, driven by a 16ms timeout on the event receiver).

Network events are emitted from `Network::resolve_target()` and the
socket-forwarding path in `network/server.rs` via an optional
`mpsc::Sender<NetworkEvent>` held on the `Network` struct. The channel
is only created when `--monitor` is active, so the non-monitor path has
zero overhead.

## Keyboard input

Two tricky issues surfaced around keyboard encoding:

### Shift+Enter in non-TUI mode

With the transparent stdin/stdout pipe, the host terminal encodes keys
using legacy Xterm encoding by default. That encoding *cannot distinguish
Shift+Enter from bare Enter* — both produce `\r`. Apps like Claude Code,
chat UIs, and editors that use Shift+Enter as "insert newline, don't
submit" therefore don't work inside the sandbox.

Options considered:
1. **Push kitty keyboard protocol from airlock + rewrite CSI-u sequences.**
   Invasive: full kitty encoding would encode Ctrl+C as `\x1b[99;5u`,
   breaking PTY line discipline which expects `\x03`. Would require a
   stdin parser that rewrites signal keys back to legacy bytes. Novel;
   no prior art.
2. **xterm `modifyOtherKeys` level 1.** Enables via `\e[>4;1m`. Level 1
   excludes keys with well-known behavior (Ctrl+letter, Tab, Backspace),
   so PTY line discipline is preserved. Shift+Enter encodes as
   `\e[27;2;13~`, which readline/bash/zsh/vim/neovim already recognize.

Went with (2) — 5-line change, zero risk.

### TUI mode keyboard

The TUI intercepts keystrokes via crossterm, so it owns the encoding
decision. Default is Xterm to preserve legacy guest-app behavior; the
kitty DISAMBIGUATE flag is used only when Shift is combined with Enter,
Backspace, Esc, or Space — the exact keys legacy Xterm cannot encode
with a modifier.

### Host `$TERM` forwarding

The guest was hardcoded to `TERM=linux`, which advertises a very limited
capability set. Forwarding the host's `$TERM` (defaulting to
`xterm-256color`) lets guest apps detect richer features and negotiate
extensions like kitty protocol through the transparent pipe.

## Scrollback and rendering

vt100 0.15 (what our plan originally specified via `"0.16"` then pulled
in as 0.15 somehow) had an overflow panic in `grid.rs:125` when the
scrollback offset exceeded the visible row count — triggered as soon as
you mouse-wheel more than `rows` lines upward. Upgrading to vt100 0.16.2
picks up the fix (`.take(rows_len)` cap + `saturating_sub` on the chain
take).

Rendering handles wide characters (CJK, emoji) explicitly: wide base
cells advance the column by 2, and continuation cells are skipped.
Ratatui's Buffer requires this or its diff renderer desyncs, which can
cause styling-escape bytes to leak to the host terminal.

Cursor rendering uses `Frame::set_cursor_position` so the host
terminal's native cursor (with the user's own customization) shows up in
the sandbox tab, rather than painting a faux block that varies in
visibility across terminals.

## Interaction details

- **F1 / F2** — switch tabs
- **F12** — toggle mouse capture. When off, the host terminal handles
  drag natively for text selection; when on, clicks/scroll wheel drive
  the TUI.
- **Ctrl+Q** — quit
- **Mouse wheel in sandbox tab** — scroll through vt100's 1000-line
  scrollback buffer. Alternate-screen apps (vim, htop) get no
  scrollback, as they don't use one on normal terminals either.
- **Click tab headers** — alternative to F1/F2 when mouse is captured

The terminal's native cursor is restored explicitly on exit
(`SetCursorStyle::DefaultUserShape` + `Show`) because ratatui's last
frame may have issued `Hide` (when network tab was active), which would
leak a hidden cursor back to the outer shell.
