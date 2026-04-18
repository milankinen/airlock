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

Two sub-tabs:

- **Requests** — one row per HTTP request the middleware handled, with
  method, path, target host:port, and whether it was allowed or denied.
- **Connections** — one row per raw TCP connection attempt, with target
  host:port and allow/deny.

Rows share the same layout: a colored `⦿` bullet (green for allowed,
red for denied), a local timestamp, the request or connection target,
and a fixed-width `Allowed` / `Denied` status column. A footer tracks
running allow/deny counts.

Switch sub-tabs with `r` / `c` or click the sub-tab labels (mouse
capture must be on — see below).

### CPU widget

One row per guest CPU core, with a gray utilization bar and a trailing
percentage that ramps green → yellow → orange → red with load. Below
the per-core rows is the guest's 1/5/15-minute load average and a
short history sparkline of the mean utilization across cores.

### Memory widget

Total and used bytes (reported the way `free` and `htop` do:
`used = MemTotal - MemAvailable`), plus a history sparkline of used%.

## Keyboard shortcuts

| Key      | Action                                                     |
|----------|------------------------------------------------------------|
| `F1`     | Switch to Sandbox tab                                      |
| `F2`     | Switch to Monitor tab                                      |
| `r`      | On Monitor tab: show Requests sub-tab                      |
| `c`      | On Monitor tab: show Connections sub-tab                   |
| `F12`    | Toggle mouse capture (release to select text with drag)    |
| `Ctrl+Q` | Quit                                                       |

## Mouse capture

`F12` toggles mouse capture. When capture is off, the host terminal
handles drag natively so you can select and copy text. When capture is
on, clicks route into the TUI — use this to click sub-tab labels or
scroll the sandbox tab's scrollback with the wheel.
