# Plan: Redesign F2 Monitor tab

## Context

The F2 tab in `airlock start --monitor` currently shows only a network
log. The redesign elevates it to a sandbox-wide "monitor" page: the
network log becomes one panel, and a new narrow right column shows
btop-style CPU and memory widgets sourced from the guest VM via a new
supervisor RPC. Layout is driven by `.tmp/tui/layout.txt` (ASCII
mockup), `.tmp/tui/cpu.png` (btop per-core bar style), and
`.tmp/tui/example.png` (connection row styling).

The rename (Network → Monitor) signals the broader scope. This change
only touches the F2 page; F1 Sandbox (embedded terminal) is unchanged.

## Target layout

```
┌──────────────────────────────────────────────────────────────┐
│ airlock sandbox monitor                    /path/of/project  │
└──────────────────────────────────────────────────────────────┘
┌ network ──────────────── always-allow ▾ ─┐┌ cpu ────── 2% ─┐
│  Requests   Connections                  ││ per-core bars  │
│ ──────────────────────────────────────── ││ load avg       │
│  ⦿  Apr 03, 11:34:24  POST /foo  :443 A  │└────────────────┘
│  ⦿  Apr 03, 11:30:12  GET  /x    :80  D  │┌ memory ── 30% ─┐
│                                          ││ total   32 GiB │
│                                          ││ used    12 GiB │
│  8 allowed   0 denied                    ││ histogram      │
└──────────────────────────────────────────┘└────────────────┘
[ bottom tab bar: F1 Sandbox   F2 Monitor ]
```

All boxes use rounded borders. The top header strip has no border; it
shows the title on the left and the project path right-aligned.

## Scope

**In scope:**
- Rename `Tab::Network` → `Tab::Monitor`, bottom label "Network" →
  "Monitor"; hotkey stays F2.
- New top header strip (title + project path).
- Two-column body: left = network panel (wide), right = CPU over
  Memory (fixed narrow width, ~32 cols).
- Network panel cosmetics: rounded border, non-functional
  "always-allow ▾" title-bar indicator, sub-tabs "Requests |
  Connections", bullet-styled rows with timestamp, footer counts.
- HTTP request events: middleware emits method/path/host/port/allowed
  along with a timestamp; Requests sub-tab shows them.
- Connections sub-tab: keeps today's connection log (TCP allow/deny),
  timestamps added.
- New supervisor `pollStats` RPC + guest collector + host-side CPU
  bars and memory histogram (rendered from scratch, no btop
  dependency).

**Out of scope:**
- Making "always-allow ▾" actually switch modes (display only).
- Expandable request detail view.
- Persisting stats history beyond the in-memory ring buffer.
- Any change to F1 Sandbox tab.

## File-level changes

### `crates/tui/src/app.rs`
- Rename `Tab::Network` → `Tab::Monitor`. Keep `F2` binding.
- Rename `network: NetworkTab` field → `monitor: MonitorTab`.

### `crates/tui/src/ui.rs`
- Bottom tab bar: change label `"Network"` → `"Monitor"`.
- When active tab is Monitor, render via new `MonitorTab::render`
  instead of inlining the network widget.

### `crates/tui/src/tabs/mod.rs`
- Rename `pub mod network;` → `pub mod monitor;` (the `monitor`
  module owns `network`, `cpu`, `memory` submodules — see below).

### New module: `crates/tui/src/tabs/monitor/`
Split the monitor tab into submodules:
- `mod.rs` — `MonitorTab` aggregate state + `render` that lays out
  header strip, left/right split, CPU/memory stack.
- `network.rs` — renamed from current `tabs/network.rs`; adds
  sub-tab state (`NetworkSubTab::Requests | Connections`), request
  log alongside the existing connection log, timestamps on entries,
  bullet + color row rendering, rounded border with title-bar mode
  indicator, footer counts.
- `cpu.rs` — `CpuWidget` renders per-core horizontal bars (btop-like
  gradient: green → yellow → red as utilization rises) and a
  load-average line. Holds a short ring buffer per core for smoothing.
- `memory.rs` — `MemoryWidget` renders total/used text and a
  histogram (ring buffer of recent used%).

### `crates/tui/src/lib.rs`
- Accept the project path (for the header strip) and a stats channel
  in `run()`. Pass both to `MonitorTab`.
- Spawn a stats-poller task that calls `supervisor.pollStats()` on a
  ~1s cadence and forwards snapshots to the monitor tab via
  `mpsc::Sender<StatsSnapshot>`.

### `crates/airlock/src/network/events.rs` (existing)
- Extend `NetworkEvent` with a new variant:
  ```rust
  Request {
      timestamp: SystemTime,
      method: String,
      path: String,
      host: String,
      port: u16,
      allowed: bool,
  }
  ```
- Add `timestamp` to the existing `Connect` variant.

### `crates/airlock/src/network/http/middleware.rs` + `http.rs`
- At the point where the middleware decides allow/deny for an HTTP
  request (see `http/middleware.rs` lines 230-288 and `http.rs`
  lines 80-125), emit a `NetworkEvent::Request` on the existing
  `event_tx` channel. Fields come from `http::request::Parts`
  (method, uri.path, host header) plus the connection's port.

### `crates/common/schema/supervisor.capnp`
Add a new method:
```capnp
interface Supervisor {
  start     @0 ...;
  shutdown  @1 ...;
  exec      @2 ...;
  pollStats @3 () -> (snapshot :StatsSnapshot);
}

struct StatsSnapshot {
  cpu         @0 :CpuStats;
  memory      @1 :MemoryStats;
  loadAverage @2 :LoadAverage;
}

struct CpuStats {
  # Per-core utilization 0..100 at snapshot time.
  perCore @0 :List(UInt8);
}

struct MemoryStats {
  totalBytes @0 :UInt64;
  usedBytes  @1 :UInt64;
}

struct LoadAverage {
  one     @0 :Float32;
  five    @1 :Float32;
  fifteen @2 :Float32;
}
```

### Guest supervisor (stats collector)
Add `pollStats` impl in the guest supervisor crate. Read `/proc/stat`
(diff between consecutive polls for per-core %), `/proc/meminfo`
(MemTotal/MemAvailable), and `/proc/loadavg`. Collector keeps the
previous `/proc/stat` sample across calls so percentages are correct
from the second call onward. Reuse `sysinfo` if it's already a guest
dep; otherwise parse directly to avoid pulling it in.

### Host wiring (`crates/airlock/src/cli/cmd_start.rs`)
- Only when `--monitor`: spawn a tokio task that ticks every 1s and
  calls the supervisor `pollStats`, pushing snapshots into an
  `mpsc::Sender<StatsSnapshot>` given to the TUI.
- Pass the resolved project path to `airlock_tui::run` for the
  header strip.

## Row styling (network panel)

- Bullet: `⦿` in `Color::Green` (allowed) / `Color::Red` (denied).
- Timestamp: `Apr 03, 11:34:24`, styled `Color::DarkGray`.
- Method + path, right-padded; long paths truncated with `…` to fit.
- `host:port` right-aligned before the status column.
- Status cell: `Allowed` (green) / `Denied` (red), right-aligned.
- Title bar: `network` on the left, `always-allow ▾` on the right of
  the border (use `Block::title_top` with `Alignment::Right` and
  styled text — no interactivity yet).

## CPU widget

- One row per core: `core-N  ▮▮▮▮▮▮░░░░░  42%`.
- Bar uses half-block characters for sub-cell precision; color ramps
  green → yellow → red based on utilization band.
- One summary line at the top: overall % derived from the mean.
- Footer line: `load 0.42 0.31 0.28`.
- Height budget: min 5 rows (collapse to summary + load only when
  narrow). Per-core bars render lazily up to available rows.

## Memory widget

- Two text rows: `total 32 GiB`, `used 12 GiB`.
- Histogram: ring buffer of the last N used%, rendered as a sparkline
  (ratatui `Sparkline`) filling the remaining height.
- Header percentage in the title bar: `memory ─── 30% ─`.

## Verification

1. `mise run lint` — workspace builds and lints clean.
2. `mise run test` — existing tests pass; add unit tests for:
   - per-core % calculation from two `/proc/stat` samples
   - `NetworkEvent::Request` emission when the HTTP middleware
     decides allow and deny.
3. Manual: `airlock start --monitor` shows the redesigned F2 page;
   F1 Sandbox unchanged.
4. Manual: run `curl` from inside the sandbox against an allowed and
   a denied host; both appear in the Requests sub-tab with correct
   color and timestamp; counts update.
5. Manual: CPU/memory values match `top` inside the guest (sanity
   check ±a few %); per-core bar count matches guest `nproc`.
6. Manual: resize the host terminal; boxes reflow, right column
   stays at ~32 cols, network panel takes the rest.

## Implementation order

1. Rename Network → Monitor (Tab enum, bottom bar label, module
   rename). No behavior change. Lints clean.
2. New `MonitorTab` layout: header strip, left/right split, placeholder
   CPU/Memory widgets. Network panel wired in unchanged.
3. Network panel cosmetics: sub-tabs, bullet rows, timestamps,
   footer, rounded border + mode indicator.
4. HTTP request events: extend `NetworkEvent`, emit from middleware,
   render in Requests sub-tab.
5. `pollStats` RPC: capnp schema, guest collector, host poller,
   snapshot channel to MonitorTab.
6. CPU widget (per-core bars, load avg, summary %).
7. Memory widget (text + sparkline).
