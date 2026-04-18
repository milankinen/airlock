# Unify monitor/non-monitor paths behind a Runtime trait

Before this change `airlock start` carried two near-parallel code paths
inside `cmd_start::run` — one for the default terminal, one for
`--monitor` TUI — with an `if monitor { ... } else { ... }` around most
of the setup (raw-mode entry, stdin client construction, event
forwarding, output relay). The same fork also leaked into `Network`,
which held an `Option<mpsc::Sender<NetworkEvent>>` for the TUI and had
to null-check on every emit.

The refactor collapses both paths behind a single trait:

```rust
pub trait Runtime {
    type Terminal: Terminal;
    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)>;
    fn signals(&self) -> anyhow::Result<SignalStream>;
    fn launch(self, &Project, &Network, rpc::Supervisor)
        -> anyhow::Result<Self::Terminal>;
}

pub trait Terminal {
    fn stdout(&mut self, bytes: &[u8]);
    fn stderr(&mut self, bytes: &[u8]);
    fn exit(self, exit_code: i32) -> i32;
}
```

Two implementations live under `crates/airlock/src/runtime/`:

- `raw_terminal.rs` — `RawTerminalRuntime` + `RawTerminal`: writes guest
  bytes straight to `stdout`/`stderr`, enters raw mode during `launch`,
  and restores cooked mode via a Drop guard on the terminal.
- `monitor_terminal.rs` — `MonitorRuntime` + `MonitorTerminal`: wraps
  `airlock_tui`; `launch` spawns the TUI thread, subscribes to network
  events, and starts the 1 Hz stats poller.

`cmd_start::run` now reads as a linear sequence:

```text
lock project  → print_preparing
prepare image → setup network
print_mounts_and_rules
boot VM       → connect supervisor
attach_stdin  → signals   → runtime.launch(&project, &network, supervisor)
supervisor.start(network)
spawn_signal_forwarder(signals, proc)
poll_proc(&proc, &mut terminal, pty_dump)
terminal.exit(code)       → supervisor.shutdown → vm.shutdown
```

Notable secondary changes this forced:

- `Runtime::enter_raw_mode` was removed — the raw runtime handles raw
  mode inside `launch`, the monitor runtime delegates to the TUI. The
  call site no longer has to know which variant is in use.
- `Network` now owns a `broadcast::Sender<NetworkEvent>` unconditionally
  and exposes `fn events() -> Receiver`. The HTTP middleware and the
  TCP server just clone the sender; there is no Optional path. Losing
  the TUI subscriber becomes a normal "no receivers" broadcast send
  that the emitter silently drops.
- `launch` must run *before* `supervisor.start(network)` because the
  latter moves `network` into the capnp RPC capability. The new
  ordering in `run` reflects that.
- The helpers `print_preparing`, `print_mounts_and_rules`,
  `spawn_signal_forwarder`, and `poll_proc` keep `run` focused on the
  orchestration; the detail functions live at the bottom of the file.
- `cmd_exec` uses `RawTerminalRuntime::new()` directly since it will
  never need the TUI path.
- `main.rs` owns clap's top-level `Program`/`Command`/`GlobalArgs`
  definitions now (previously in `cli.rs`). All command entry points
  renamed to `main()` for symmetry.
- `terminal/` module renamed to `runtime/` to match the new role
  (`terminal` now describes only the output-sink trait).

No behavior changes — the flag surface, the TUI, the guest contract,
and the on-disk layout are all identical. The commit is a pure
refactor that makes the next monitor-side additions (HTTP request
detail view, stats history) easier to add without re-branching the
orchestration code.
