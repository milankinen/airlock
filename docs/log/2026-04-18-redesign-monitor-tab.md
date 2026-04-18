# Redesign the `--monitor` F2 page into a sandbox-wide monitor

## Motivation

F2 in the first cut of `airlock start --monitor` was a single-purpose
"Network" tab — a scrolling list of TCP allow/deny events. Useful, but
narrow: you still had no visibility into what resources the sandbox was
using, and HTTP traffic (the middleware decides per-request) was invisible.

This change reframes F2 as the **Monitor tab**: the network log is one
panel on the left, and a narrow right column shows btop-style CPU and
memory widgets sourced live from the guest VM. HTTP requests now get their
own sub-tab alongside the existing connection log.

F1 Sandbox is untouched.

## Layout

```
┌ airlock sandbox monitor                   /path/of/project ┐
└────────────────────────────────────────────────────────────┘
┌ network ────────── always-allow ▾ ─┐┌ cpu ───────── 32% ─┐
│  Requests   Connections            ││ c0 █████░░░░  42%  │
│ ──────────────────────────────────│││ c1 ██░░░░░░░  18%  │
│  ⦿  Apr 18, 11:34:24  POST /foo …  ││ load 0.42 0.31 …   │
│  ⦿  Apr 18, 11:30:12  :443  Denied ││ ▂▃▄▅▄▃▂▁▁▂▃▅       │
│                                    │└────────────────────┘
│  8 allowed   0 denied              │┌ memory ──── 30% ─┐
└────────────────────────────────────┘│  total  32 GiB   │
                                      │  used   12 GiB   │
                                      │ ▁▁▂▂▃▃▄▄▄▄▄      │
                                      └──────────────────┘
```

The right column has a fixed ~32-col width; the network panel takes the
rest. Boxes size to their content and sit at the top of the column — they
don't stretch to fill the terminal.

## Stats pipeline

A new Cap'n Proto method `Supervisor.pollStats()` returns a `StatsSnapshot`
with per-core CPU %, total/used memory bytes, and 1/5/15 load averages.

The guest implementation (`crates/airlockd/src/stats.rs`) parses `/proc`
directly rather than pulling in a `sysinfo` dependency:

- `/proc/stat` — per-core idle/total jiffies. We keep the previous sample
  in a `Collector` so diffs yield correct percentages from the second
  call onward; the first call returns zeros.
- `/proc/meminfo` — `MemTotal` and `MemAvailable`, with `used = total -
  available` to mirror what `free`/`htop` report.
- `/proc/loadavg` — first three floats.

On the host, `cmd_start` spawns a tokio task (only in `--monitor` mode)
that ticks every 1s, calls `poll_stats`, and forwards snapshots into the
TUI via the existing `TuiSender`. The TUI thread routes them to
`MonitorTab::apply_stats`, which updates the CPU and memory states and
pushes the new sample into each widget's 120-entry ring buffer.

## Widget rendering

Both widgets share a custom per-column vertical histogram
(`tabs/monitor/histogram.rs`) rather than ratatui's `Sparkline`. Reason:
`Sparkline` renders an empty cell for 0% samples, which made the
histogram visually disappear when CPU was idle or memory hadn't been
exercised. The custom renderer floors the fill to at least one
1/8-block sub-cell so a baseline is always visible, and uses the full
`▁▂▃▄▅▆▇█` progression for 1/8-block precision.

The CPU widget renders one row per core: `c0 ████░░░  42%`. Bars use
`█` + `▌` half-block for sub-cell precision but stay gray — the trailing
percentage carries the color (green <50, yellow <70, orange <85, red).
Tying color to the quiet tail keeps the bars themselves from flickering
into red and dominating peripheral vision when one core briefly spikes.

The memory widget shows `total` / `used` text plus a cyan history
sparkline of used%.

## Network panel

The existing network tab picked up sub-tabs. `NetworkSubTab::Requests`
shows a new `RequestEntry` (method, path, host, port, allowed, timestamp)
emitted from the HTTP middleware; `NetworkSubTab::Connections` keeps the
original TCP connect log. Both share a row renderer
(`tabs/monitor/network/row.rs`) that composes `⦿ timestamp · left · right
· status`, and a footer that reports allow/deny counts.

Sub-tab switching works via `r`/`c` keys or by clicking the labels. For
the click path, each sub-tab label's rect is captured at render time into
a `Cell<Option<Rect>>` on `NetworkTab` and consulted by the mouse handler
— interior mutability lets the widget keep a `&self` render borrow.

The status column is fixed at 7 chars (the width of `Allowed`), left-
aligned, so `Denied` and `Allowed` start at the same column across rows.

## Module layout

```
crates/tui/src/tabs/
    mod.rs
    monitor/
        mod.rs         -- MonitorTab + two-column layout
        cpu.rs         -- CPU per-core bars + history
        memory.rs      -- total/used + history
        histogram.rs   -- shared per-column vertical histogram
        network/
            mod.rs         -- NetworkTab state + sub-tabs
            chrome.rs      -- border + mode indicator + sub-tab header
            row.rs         -- shared row renderer
            requests.rs    -- Requests sub-tab
            connections.rs -- Connections sub-tab
            footer.rs      -- allow/deny counts
```

`tabs/network.rs` was removed; its logic now lives under `tabs/monitor/
network/` as part of the Monitor tab.
