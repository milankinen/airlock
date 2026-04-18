# Monitor mode

The `--monitor` (`-m`) flag opens a tabbed TUI control panel alongside
the sandbox shell. It's most useful when you want to observe what the
sandbox is doing — which outbound connections it's making, which are
being blocked by policy, and how it's using CPU and memory.

```bash
airlock start --monitor
```

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
a gray header row naming the columns.

- **Requests** (default) — one row per HTTP request the middleware
  handled. Columns: `Received at`, `Endpoint` (method + path),
  `Target` (host:port), `Result` (`Allowed` green / `Denied` red).
  Denied HTTP requests are included here too: the proxy captures the
  full request before responding with `403 Forbidden` instead of
  refusing at the TCP layer, so you can see exactly what was attempted.
- **Connections** — one row per raw TCP connection. Columns: a colored
  `⦿` bullet, `Target` (host:port, white), `Connected at`,
  `Disconnected at`, `Result`. The bullet signals connection lifecycle:
  **green** means the connection is still open (`Disconnected at` is
  blank), **gray** means it closed, **red** means the connection was
  denied. A footer tracks running allow/deny counts.

Use `↑` / `↓` to move the row selection (PgUp/PgDn, Home, End also
work), and press `Enter` to open a **details** sub-tab with the full
snapshot — including captured request headers for HTTP. Close it with
`Esc`, `x`, or the `×` in the tab label.

Switch sub-tabs with `r` / `c` or click the sub-tab labels (mouse
capture must be on — see below).

### Policy selector

The top-right of the network panel shows the active policy (e.g.
`policy: Deny by default ▾`). Press `p` or click the label to open a
dropdown and pick a new policy live — the change takes effect on the
next connection the sandbox makes. Colors hint at the strictness:
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

| Key                     | Action                                                |
|-------------------------|-------------------------------------------------------|
| `F1`                    | Switch to Sandbox tab                                 |
| `F2`                    | Switch to Monitor tab                                 |
| `r`                     | On Monitor tab: show Requests sub-tab                 |
| `c`                     | On Monitor tab: show Connections sub-tab              |
| `↑` / `↓`               | Move row selection in Requests / Connections          |
| `PgUp` / `PgDn`         | Jump the selection a page at a time                   |
| `Home` / `End`          | Jump to the newest / oldest entry                     |
| `Enter`                 | Open the selected row in a details sub-tab           |
| `Esc` / `x`             | Close the details sub-tab                             |
| `p`                     | On Monitor tab: open the policy dropdown              |
| `q` / `Ctrl+D`          | On Monitor tab: ask the sandbox process to exit       |
| `F12`                   | Toggle mouse capture (release to select text)         |

## Mouse capture

`F12` toggles mouse capture. When capture is off, the host terminal
handles drag natively so you can select and copy text. When capture is
on, clicks route into the TUI — use this to click sub-tab labels or
scroll the sandbox tab's scrollback with the wheel.
