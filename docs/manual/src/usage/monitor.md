# Monitor dashboard

The `--monitor` (`-m`) flag opens a tabbed TUI control panel alongside
the sandbox shell. It's most useful when you want to observe what the
sandbox is doing — which outbound connections it's making, which ones
the policy blocks, and how it's using CPU and memory.

```bash
airlock start --monitor
```

![Monitor dashboard](./monitor.png)

## Tabs

- **F1 — Sandbox**: the embedded VM terminal, with 1000 lines of
  mouse-wheel scrollback. Alternate-screen apps (vim, htop, …) use the
  guest's own screen and don't have scrollback, just like in a normal
  terminal.
- **F2 — Monitor**: sandbox-wide observability. The left side shows a
  network panel with **Requests** (HTTP method, path, host, port,
  allow/deny) and **Connections** (raw TCP allow/deny) sub-tabs. The
  right side shows CPU and memory widgets sourced from the guest VM
  once per second.

## Monitor tab

### Network panel

Two sub-tabs (newest entries at the top, up to 100 of each). Both have
a grey header row naming the columns.

- **Requests** (default) — one row per HTTP request the middleware
  handled. Columns: `Received at`, `Endpoint` (method + path),
  `Target` (host:port), `Result` (`Allowed` green / `Denied` red).
  This list includes denied HTTP requests too. The proxy captures the
  full request before it responds with `403 Forbidden`, instead of
  refusing at the TCP layer. So you can see exactly what the sandbox
  attempted.
- **Connections** — one row per raw TCP connection. Columns: a coloured
  `⦿` bullet, `Target` (host:port, white), `Transferred`, `Connected at`,
  `Disconnected at`, `Result`. The bullet signals connection lifecycle:
  **green** means the connection is still open (`Disconnected at` is
  blank), **grey** means it closed, **red** means the connection was
  denied. A footer tracks running allow/deny counts.

`Transferred` shows `↑ 4.2MB ↓ 61MB` — how much the sandbox sent and
received on that connection, updating live. It's a measure of network
traffic rather than of file size, so it counts protocol overhead too.
It will read a little above the size of the actual download. Narrow
terminals drop the column to leave room for `Target`.

Use `↑` / `↓` to move the row selection (PgUp/PgDn, Home, End also
work). Press `Enter` to open a **details** sub-tab with the full
snapshot. Close it with `Esc`, `x`, or the `×` in the tab label.

For an HTTP request the details view shows the headers the sandbox
sent, and the response status and headers that came back. Requests
still waiting on a reply show `(no response yet)`. For a connection it
shows exact byte counts.

When the snapshot is taller than the panel, `↑` / `↓` (and PgUp/PgDn,
Home/End, or the mouse wheel) scroll it.

Switch sub-tabs with `r` / `c`, or click the sub-tab labels.

### Policy selector

The top-right of the network panel shows the active policy (e.g.
`policy: Deny by default ▾`). Press `p` or click the label to open a
dropdown and pick a new policy live. The change takes effect on the
next connection the sandbox makes. Colours hint at the strictness:
green (`Always allow`), blue (`*-by-default`), red (`Always deny`).

### CPU widget

One row per guest CPU core, with a utilization bar and trailing
percentage that both ramp green → yellow → orange → red with load.
Below the per-core rows is the guest's 1/5/15-minute load average and
a short history sparkline of the mean utilization across cores.

### Memory widget

Total and used bytes (reported the way `free` and `htop` do:
`used = MemTotal - MemAvailable`), plus a history sparkline of used%.

## Keyboard shortcuts

| Key             | Action                                          |
|-----------------|-------------------------------------------------|
| `F1`            | Switch to Sandbox tab                           |
| `F2`            | Switch to Monitor tab                           |
| `r`             | On Monitor tab: show Requests sub-tab           |
| `c`             | On Monitor tab: show Connections sub-tab        |
| `↑` / `↓`       | Move row selection, or scroll the details view  |
| `PgUp` / `PgDn` | Same, a page at a time                          |
| `Home` / `End`  | Jump to either end of the list or details view  |
| `Enter`         | Open the selected row in a details sub-tab      |
| `Esc` / `x`     | Close the details sub-tab                       |
| `p`             | On Monitor tab: open the policy dropdown        |
| `q`             | On Monitor tab: switch back to Sandbox tab      |
| `Ctrl+D`        | On Monitor tab: ask the sandbox process to exit |

## Personal settings

Buffer caps, terminal scrollback, and key bindings are personal
preferences — they live in `~/.airlock/settings.toml`, not in the
per-project `airlock.toml`. All fields default to the values used
here, so there's nothing to set unless you want to change them.

### Buffer caps and scrollback

The Monitor tab keeps a rolling buffer of recent network activity.
Once either buffer is full, airlock drops the oldest entries to make
room for new ones (this does not affect the lifetime allowed/denied
counters). The Sandbox tab's vt100 terminal keeps a separate
scrollback buffer.

```toml
[monitor.buffers]
http = 100   # default; max HTTP request entries
tcp = 100   # default; max TCP connection entries
scrollback = 1000  # default; vt100 scrollback rows for the Sandbox tab
```

Larger buffers keep more history visible in long sessions. A larger
`scrollback` lets you scroll further back into long build output.
Both are in-memory and don't persist across sandbox restarts.

### Key bindings

Shortcuts live in `[monitor.keys]` as an action-name → key(s) map.
Each value is either a single key string or a list of keys. You
override only the actions you list here — the rest keep their
defaults. A single `back = "esc"` is a complete config.

```toml
[monitor.keys]
switch-sandbox = "f1"               # force-switch to Sandbox tab
switch-monitor = "f2"               # force-switch to Monitor tab
back = "q"                # step back: list → Sandbox tab; modal → close
cancel = ["esc", "x"]       # dismiss the topmost modal
confirm = "enter"            # open details / apply policy
kill-sandbox = "ctrl+d"           # send SIGHUP+SIGTERM to the sandbox process
select-up = "up"
select-down = "down"
select-page-up = "pageup"
select-page-down = "pagedown"
select-newest = "home"
select-oldest = "end"
toggle-sub-tab = ["tab", "left", "right"]   # Requests ↔ Connections
select-requests = "r"
select-connections = "c"
open-policy = "p"                # open the network-policy dropdown
```

#### Key string format

`[<modifier>+]*<key>`. Modifiers (case-insensitive): `ctrl`, `alt` (or
`option` / `meta`), `shift`, `super` (or `cmd` / `command`). Keys:

- single ASCII chars: `q`, `1`, `+`, `?`, …
- named keys: `enter`, `esc` / `escape`, `tab`, `backspace`, `delete`,
  `space`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`,
  `pagedown`, `f1`–`f12`

Examples: `q`, `ctrl+d`, `shift+tab`, `f2`, `alt+enter`.

airlock treats `shift+<letter>` the same as the lowercase letter.
Terminals emit shifted letters as plain uppercase chars without a
separate modifier flag, so a `shift+a` binding would never fire. Use a
different modifier or key if you want a shifted variant.

#### Action semantics

Actions are context-aware — `back` and `confirm` mean different things
depending on what's open:

| Action    | List view             | Details pane  | Policy dropdown          |
|-----------|-----------------------|---------------|--------------------------|
| `back`    | switch to Sandbox tab | close details | close dropdown           |
| `cancel`  | (no-op)               | close details | close dropdown           |
| `confirm` | open details          | (no-op)       | apply highlighted policy |

The navigation actions (`select-*`, `toggle-sub-tab`, `open-policy`,
`kill-sandbox`) only apply on the Monitor tab. The Sandbox tab is full
keystroke passthrough — only the two `switch-*` shortcuts are intercepted.

airlock reports invalid key strings (unknown modifier, unknown key
name) when the sandbox starts. It refuses to start the TUI rather
than silently drop a binding.

## Selecting text

Because the monitor UI holds your terminal's mouse, dragging to select
text doesn't work the way it does in a normal terminal window.
Your terminal has an escape hatch for exactly this: **hold a modifier
while you drag**, and it takes the mouse back for that one gesture. Then
copy as usual (`Cmd+C` on macOS, usually `Ctrl+Shift+C` on Linux).

Which modifier depends on your terminal:

| Terminal                                                                          | Hold                                 |
|-----------------------------------------------------------------------------------|--------------------------------------|
| iTerm2                                                                            | `Option`                             |
| Terminal.app                                                                      | `Fn`                                 |
| VS Code                                                                           | `Option` on macOS, `Shift` elsewhere |
| xterm, GNOME Terminal, Konsole, kitty, Alacritty, WezTerm, Windows Terminal, tmux | `Shift`                              |

You don't have to memorize this. Click anywhere in the monitor and it
tells you, in the bar at the bottom:

```
Hold Option to select text
```

The hint appears for a couple of seconds after each click and then
disappears. It names the modifier for the terminal airlock detects
you're using. If it can't tell, it says `Shift`, which is right almost
everywhere.

The same applies in the Monitor tab's **details** view when you want to
copy request or response headers out — hold the modifier, drag, copy.
