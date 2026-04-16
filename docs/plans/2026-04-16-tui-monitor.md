# Plan: TUI monitoring control panel for `airlock start --monitor`

## Context

The user wants a ratatui-based TUI that wraps the sandbox terminal in a
tabbed control panel. Two tabs initially: **Sandbox** (embedded terminal)
and **Network** (live connection log with policy display). A third
**Resources** tab (btop inside VM) is deferred.

The TUI should support mouse (iTerm2 SGR mode) and keyboard shortcuts for
tab switching and actions. The PTY abstraction should be designed so the
sandbox terminal can run with or without TUI decoration, and so tabs can
later be attached to a running sandbox via CLI RPC.

## Architecture

### New crate: `crates/tui/`

Separate crate to keep ratatui/vt100 dependencies out of the core CLI.
The airlock binary imports it and calls into it when `--monitor` is passed.

**Dependencies:** `ratatui`, `ratatui-crossterm`, `vt100`, `crossterm`

### PTY abstraction layer

Currently, the host-side PTY pipeline is:
```
tokio::io::stdin() → rpc::Stdin → [vsock] → supervisor → PTY → process
process → PTY → supervisor → [vsock] → rpc::Process.poll() → stdout
```

For the TUI, we need to intercept this: instead of reading from real stdin
and writing to real stdout, the TUI's terminal widget sends/receives bytes.

**Key abstraction:** `TerminalSink` trait

```rust
/// Receives PTY output for display.
pub trait TerminalSink {
    /// Process output bytes (stdout/stderr).
    fn write(&mut self, data: &[u8]);
    /// Process exited.
    fn exit(&mut self, code: i32);
}
```

For the raw (non-TUI) path, `TerminalSink` writes directly to stdout (current behavior).
For the TUI path, it feeds bytes into the `vt100::Parser` that backs the terminal widget.

**Input direction:** The TUI event loop captures keyboard events. When the
Sandbox tab is focused, keystrokes are forwarded to the PTY via a channel
that the RPC `Stdin` server reads from (instead of `tokio::io::stdin()`).

### Data flow with TUI

```
┌─ TUI Event Loop ──────────────────────────────────────┐
│  crossterm::event::read()                              │
│    ├─ Key event (sandbox focused) → pty_input_tx       │
│    ├─ Key event (tab switch) → update active tab       │
│    ├─ Mouse event → handle click/scroll                │
│    └─ Resize event → resize terminal + vt100 parser    │
│                                                         │
│  pty_input_rx → rpc::Stdin → supervisor → guest PTY    │
│  rpc::Process.poll() → vt100::Parser → terminal widget │
│                                                         │
│  network_events_rx → network log state → network tab   │
└─────────────────────────────────────────────────────────┘
```

### Network event channel

Add an `event_tx: Option<mpsc::Sender<NetworkEvent>>` to the `Network`
struct. When present, `server.rs` sends events on each connection:

```rust
pub enum NetworkEvent {
    Connect { host: String, port: u16, allowed: bool },
    // Future: RequestComplete, Error, etc.
}
```

The TUI's network tab receives from the corresponding `event_rx` and
maintains a scrollable log with counters.

## Detailed changes

### Phase 1: Crate setup and CLI flag

1. Create `crates/tui/` with `Cargo.toml`:
   - Dependencies: `ratatui = "0.30"`, `vt100 = "0.17"`, `crossterm = "0.29"`,
     `tokio`, `airlock-protocol`
   - Re-export a public `run()` entry point

2. Add to workspace `Cargo.toml`:
   - `members` list
   - `airlock-tui = { path = "crates/tui" }` workspace dep
   - Add `ratatui`, `vt100` to workspace deps

3. Add `--monitor` flag to `StartArgs` in `cmd_start.rs`

4. Add `airlock-tui` dependency to `crates/airlock/Cargo.toml`

### Phase 2: PTY abstraction

5. **`crates/tui/src/pty.rs`** — Define `TerminalSink` trait and a
   `TuiTerminalSink` that wraps `vt100::Parser`:
   ```rust
   pub struct TuiTerminalSink {
       parser: vt100::Parser,
   }
   impl TuiTerminalSink {
       pub fn screen(&self) -> &vt100::Screen { self.parser.screen() }
       pub fn resize(&mut self, rows: u16, cols: u16) { self.parser.set_size(rows, cols); }
   }
   impl TerminalSink for TuiTerminalSink {
       fn write(&mut self, data: &[u8]) { self.parser.process(data); }
       fn exit(&mut self, _code: i32) { /* mark as exited */ }
   }
   ```

6. **`crates/tui/src/input.rs`** — Channel-based stdin replacement:
   ```rust
   pub struct TuiStdin {
       rx: mpsc::Receiver<TuiInputEvent>,
   }
   pub enum TuiInputEvent {
       Data(Vec<u8>),
       Resize(u16, u16),
   }
   ```
   This implements the same `stdin::Server` RPC interface but reads from
   a channel instead of `tokio::io::stdin()`.

### Phase 3: Network events

7. **`crates/airlock/src/network/events.rs`** — Define `NetworkEvent` enum
   and add `event_tx` to `Network` struct. Emit events from `server.rs`
   on connect/deny.

8. The `network::setup()` function takes an optional `event_tx` parameter.
   When `--monitor` is active, `cmd_start.rs` creates the channel and
   passes the sender to `network::setup()`, receiver to the TUI.

### Phase 4: TUI app

9. **`crates/tui/src/app.rs`** — Main `App` struct:
   ```rust
   pub struct App {
       active_tab: Tab,
       sandbox: SandboxTab,
       network: NetworkTab,
       should_quit: bool,
   }
   pub enum Tab { Sandbox, Network }
   ```

10. **`crates/tui/src/tabs/sandbox.rs`** — Sandbox tab:
    - Holds reference to `TuiTerminalSink` (for rendering vt100 screen)
    - Holds `mpsc::Sender<TuiInputEvent>` (for forwarding keystrokes)
    - Renders terminal content using vt100 screen cells → ratatui buffer
    - When focused, all key events (except tab-switch shortcut) forwarded

11. **`crates/tui/src/tabs/network.rs`** — Network tab:
    - `NetworkTab` state: `Vec<NetworkLogEntry>`, counters, scroll position
    - Receives from `mpsc::Receiver<NetworkEvent>`
    - Renders:
      - Policy display (top)
      - Request log table (scrollable)
      - Summary bar (allowed/denied counts)
    - Tab header pill: shows request count, turns red if any denied

12. **`crates/tui/src/ui.rs`** — Layout and rendering:
    - Top bar: tab titles with pill indicators, clickable
    - Body: active tab content fills remaining space
    - Bottom bar: keyboard shortcut hints
    - Tab header rendering with colored pills

### Phase 5: Event loop

13. **`crates/tui/src/lib.rs`** — Main `run()` function:
    ```rust
    pub async fn run(
        proc: rpc::Process,
        stdin_tx: mpsc::Sender<TuiInputEvent>,
        network_rx: mpsc::Receiver<NetworkEvent>,
        policy: Policy,
    ) -> anyhow::Result<i32>
    ```
    - Enter alternate screen + enable mouse capture
    - Spawn background task: poll `proc` for output → feed to `TuiTerminalSink`
    - Main loop:
      - Poll crossterm events (with ~16ms timeout for ~60fps)
      - Drain network events from channel
      - Handle key/mouse events
      - Render frame
    - On process exit: show exit code, wait for user keypress, then exit
    - Restore terminal on drop

### Phase 6: Integration in cmd_start.rs

14. **`crates/airlock/src/cli/cmd_start.rs`**:
    - If `--monitor`: create channels, pass `event_tx` to network setup,
      create `TuiStdin` (channel-based), call `tui::run()` instead of
      the raw poll loop
    - If no `--monitor`: existing behavior unchanged

### Keyboard shortcuts (initial)

| Key | Action |
|-----|--------|
| `Ctrl+1` or `F1` | Switch to Sandbox tab |
| `Ctrl+2` or `F2` | Switch to Network tab |
| `Ctrl+q` | Quit (send SIGTERM to process) |
| `↑/↓/PgUp/PgDn` | Scroll in network log (when network tab active) |

### Mouse support

- Click on tab header → switch tab
- Scroll wheel in network log → scroll
- All mouse events in sandbox tab → forwarded to PTY as escape sequences

## Files to create

1. `crates/tui/Cargo.toml`
2. `crates/tui/src/lib.rs` — entry point, event loop
3. `crates/tui/src/app.rs` — App state
4. `crates/tui/src/pty.rs` — vt100-backed terminal sink
5. `crates/tui/src/input.rs` — channel-based stdin for RPC
6. `crates/tui/src/ui.rs` — layout and rendering
7. `crates/tui/src/tabs/mod.rs`
8. `crates/tui/src/tabs/sandbox.rs` — embedded terminal tab
9. `crates/tui/src/tabs/network.rs` — network monitor tab

## Files to modify

1. `Cargo.toml` — add tui crate to workspace
2. `crates/airlock/Cargo.toml` — add airlock-tui dependency
3. `crates/airlock/src/cli/cmd_start.rs` — add `--monitor` flag, TUI branch
4. `crates/airlock/src/network.rs` — add `event_tx` to Network
5. `crates/airlock/src/network/server.rs` — emit NetworkEvents

## Rendering the vt100 screen

`tui-term` provides a `PseudoTerminal` widget but it may be simpler to
render directly from `vt100::Screen` → ratatui `Buffer`. The screen gives
us cell-by-cell access (char, fg, bg, bold, etc). We iterate rows/cols
and write to the ratatui buffer. This avoids the `tui-term` dependency
and gives us full control.

If `tui-term` proves easier to integrate, we can use it — but the vt100
screen-to-buffer conversion is straightforward (~50 lines).

## Verification

1. `mise run lint` — all crates compile clean
2. `mise run test` — existing tests pass
3. Manual: `airlock start --monitor` shows tabbed TUI
4. Manual: typing in sandbox tab works
5. Manual: network tab shows connection log
6. Manual: mouse click switches tabs
7. Manual: Ctrl+1/Ctrl+2 switches tabs
