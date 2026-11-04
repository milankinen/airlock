# Detached sandbox host: auto start and share for exec/acp

## Context

Today `airlock start` owns the VM in its foreground process. `airlock exec`
and `airlock acp` only connect to `<sandbox>/cli.sock` and fail when no
`start` runs. Goal: exec/acp start the sandbox on demand with the last-run
config, all sessions share one VM, and the VM stops when the last session
ends. `start` re-applies config, so it fails when the sandbox runs. New
`airlock stop` forces shutdown.

User decisions:
- A detached host process ALWAYS owns the VM. `start` also uses it and runs
  its main command as one session over cli.sock. Monitor TUI gets data over RPC.
- No grace period. Stop immediately when the last session ends.
- `network forward` connections do not hold the sandbox and do not auto-start.
  They close at shutdown.
- exec/acp error if `start` never created the sandbox (also with airlock.toml).

## High-level design

1. **Host process** `airlock sandbox-daemon` (hidden clap command). Only owner of a VM.
   Holds the sandbox flock. Resolves config from run.json `last_run_config`
   (`ApprovalMode::Refuse`), loads the pinned sandbox image (no pull),
   `project::open` with the stored guest cwd, boots, serves cli.sock, counts
   sessions. Stops when the last session ends, on `stop` RPC, or on SIGTERM.
   Logs to `<sandbox>/airlock.log`.
2. **Launcher** (shared by start/exec/acp). Takes the sandbox flock itself and
   passes the same open file description to the host (`pre_exec`: `setsid`,
   `dup2(lock_fd, 3)`), so no other process can take the lock in between.
   Pipes: stdin = JSON handoff (vault passphrase, only for encrypted-file
   vault), stdout = `ready` / `error <text>` line, stderr = boot progress
   copied to launcher stderr until EOF. Ctrl+C during boot wait → SIGTERM to
   host → boot returns `Interrupted` → exit 130. Background `child.wait()`
   reaps the host.
3. **Session leases.** RPC `openSession() -> union { session :Session;
   stopping :Void }`. Lease ends on `Session.release`, cap drop, or
   connection drop. Single-threaded LocalSet gives atomicity: `openSession`
   checks `stopping` and increments with no `await` between. The lease
   `Drop` decrements and, at 0, sets `stopping` in the same call. Then
   shutdown: stop services (aborts cli server, removes cli.sock, closes all
   connections incl. forwards), daemons, sync, VM, process exit (releases
   flock). Before the first session, the 0-count rule does not apply;
   `FIRST_SESSION_TIMEOUT` (30 s after ready) stops a host whose launcher
   died. This is a safety net, not a grace period.
4. **Race: connect while stopping.** Client sees `stopping`, ECONNREFUSED,
   NotFound, or disconnect during the call → polls `lock_if_idle` until not
   `Running` → `Held(file)`: spawns a new host. Overall deadline
   `SHUTDOWN_TIMEOUT` + boot time, then clear error.
5. **start end.** After its main process exits, `start` calls
   `Session.release() -> (last :Bool)`. When `last`, it waits for the lock to
   be free before it exits. So disk sync is done at exit as today, and
   back-to-back `start -- cmd` (all VM bats tests) keep working. exec/acp do
   not wait.
6. **Monitor over RPC.** `Runtime::launch` takes `MonitorSource {
   project_path, control: Arc<dyn NetworkControl>, events rx, stats rx }`
   instead of `(&Project, &NetworkHandle, Supervisor)`. Host side: `monitor`
   RPC subscribes to `NetworkHandle::events()`, batches (≤256 / 50 ms),
   polls stats 1/s while attached, pushes policy changes. A slow sink must
   never block the host network path: bounded queue, drop on lag (as the
   broadcast `Lagged` does today). Client `RemoteNetworkControl` caches
   policy (`Arc<Mutex<Policy>>`), sends `setPolicy` through a channel to a
   local RPC task (fire-and-forget, log errors).
7. **Lock adoption.** `SandboxLock::adopt(fd, ..)` checks dev/ino of the fd vs
   `<sandbox>/lock` (via `VerifiedPath`, no follow). It sets CLOEXEC on the
   fd, so VM helper processes do not inherit the lock. It writes the host
   PID by opening the lock path for write (the inherited fd from
   `lock_if_idle` is read-only).

8. **Approvals in the host.** The host makes the approver of the
   `approvals.dialogs` setting, so the desktop dialogs run in the host (with
   the env of the process that started it). The monitor dialog fallback:
   the `monitor` call puts a remote dialog for its sink into the host
   `MonitorSlot`. The drop of its `MonitorControl` (release or disconnect)
   removes it. `MonitorSink.approve` carries the clean request text and the
   time left. The client shows it in the TUI and drops it at the deadline.
   The host drops the call when its receiver times out. An RPC error gives
   the "monitor stopped" deny.

Config notes: the only CLI override is `--network` (`ConfigOverrides`). The
host gets `--network` only from `start`. An exec-started host uses the
configured policy. `[env]` `${VAR}` and DISPLAY/WAYLAND/D-Bus come from the
process that starts the host. Document this.

## Detailed plan (phases, sequential)

Phases 1 and 2 both touch `cli_server.rs` and `cli.capnp`, so all phases run
in order. Each phase: opus producer, sonnet critic, gates
`mise format`, `mise run lint`, `mise run test`, then WIP commit.

### Phase 1 – session tracker and control RPCs
- `app/airlock-common/schema/cli.capnp`: `openSession @4`, `stop @5`,
  `status @6 () -> (sessions :UInt32, stopping :Bool)`, `notices @7 () ->
  (list :List(Text))` (browser notices for start; capnp refuses holes in
  method ordinals), `interface Session {
  release @0 () -> (last :Bool); }`.
- New `app/airlock-cli/src/host/sessions.rs`: `SessionTracker { count,
  opened_once, stopping, notify }`, `open() -> Option<Lease>`, RAII `Lease`,
  `async fn wait_stop(&self, first_timeout) -> StopReason { LastEnded,
  NoFirstSession, Requested }`. Unit tests for transitions.
- `cli_server.rs`: `serve(.., tracker: Rc<SessionTracker>)`, `SessionImpl`
  holds a `Lease`. Empty exec `cwd` maps to the sandbox cwd.
- Tests `cli/tests/test_session_lease.rs` (reuse the fake supervisor from
  `test_network_forward.rs`; move it to `test_cfg` if shared):
  `last_session_release_stops_host`, `open_session_while_stopping_returns_stopping`,
  `forward_connection_does_not_hold_host`, `forward_connection_closes_at_shutdown`,
  `stop_rpc_ends_open_sessions`.

### Phase 2 – monitor over RPC
- New `app/airlock-common/schema/monitor.capnp`: event structs for
  `ConnectInfo`, `DisconnectInfo`, `TrafficInfo`, `RequestInfo`,
  `ResponseInfo`, `StatsSnapshot`; `MonitorSink` (stream methods),
  `MonitorControl.setPolicy`. In cli.capnp: `monitor @8 (sink) ->
  (control, projectPath, policy)`. Conversions in `rpc/monitor.rs`.
- Host side `host/monitor.rs`; client side `runtime/monitor_remote.rs`.
- New `Runtime::launch(self, source: MonitorSource)` in
  `runtime/{mod,host,monitor_terminal,raw_terminal}.rs`. Interim: a local
  source from `NetworkHandle` in `interactive.rs`, removed in Phase 5.
- Test `cli/tests/test_monitor_rpc.rs`: network events and policy change
  cross the RPC.
- Manual: `mise airlock -- start -m` network tab, stats, policy change.

### Phase 3 – host process
- First: macOS spike. Check `vm/apple.rs` (Virtualization.framework) runs
  from a `setsid` process with no terminal. If not, stop and report.
- `project/mod.rs`: `SandboxLock::adopt`, `IdleLock::Held(File)` gives an
  inheritable fd. Unit test: fd of another file is refused.
- `oci/mod.rs`: `load_sandbox_image(data_dir, sandbox_dir)` (wraps
  `read_ready_image`, made `pub(crate)`; no network).
- `start/config_change.rs`: make `resolve_last` usable with a small input
  type. Host errors: no last config → reuse `messages::cli::no_last_run_config`;
  missing image; pending installs; pack approval refused. All say "run
  airlock start".
- Vault: `PassphraseSource::Fixed(String)` for the host;
  `Vault::handoff_passphrase()` in the launcher (env var or prompt on TTY;
  acp without TTY and without env var fails with a clear error).
- New `host/{mod,handoff,launch}.rs`: `run(args, ctx)`, `HostHandoff`, ready
  protocol, `spawn_host(lock: File, &HostOptions)`. `current_exe` via
  `VerifiedPath::new_follow` (as `cmd_self_update.rs:69`). Host cwd = sandbox
  dir. Hidden args `--project --sandbox-dir --lock-fd --log-level --verbose
  [--network]`: open with `VerifiedPath::new`, check that the sandbox dir is
  under `context.data` and that the registry maps it to `--project`.
  `/dev/null` redirect after ready: helper in `util/verified_path.rs` or
  `util/safe_fs.rs` (only places that may allow `clippy::disallowed_*`).
  Panics and tracing go to airlock.log before ready.
- `sandbox/hosted.rs`: `run_hosted(project, image, opts, ready)`, split from
  `run_interactive`: services, browser, network, env, daemons, masks,
  clipboard, MCP, boot → `serve_cli(tracker)` → ready →
  `select!(tracker.wait_stop, SIGTERM/SIGINT)` → `services.detach` →
  `vm.shutdown`. Shutdown errors go to airlock.log.
- `cli/cmd_host.rs`, `Command::Host` (hidden) in `main.rs`.
- Test `cli/tests/test_host_launch.rs`: child inherits lock fd, launcher
  drop keeps the lock; `error` line is reported.

### Phase 4 – exec, acp, network forward clients
- `cli/sandbox_client.rs`: `find_sandbox` → `LocatedSandbox { sock, found,
  guest_root, running }`: first candidate whose sock exists, or that has
  content and `last_run_config`. Keep `follow_git_worktree` and the
  "attached to sandbox" message. `locate_running` keeps old behavior for
  `network forward`. `open_session(..)` retry loop (design item 4); prints
  `STARTING_SANDBOX` / `WAITING_FOR_SANDBOX` to stderr.
  Behavior change: a nested project with a stopped sandbox wins over a
  running parent sandbox. Document.
- New `cli/attach.rs`: `run_attached(service, runtime, argv, cwd, env)
  -> i32`, extracted from `cmd_exec.rs:60-86` (signals, resize, pty, pipe
  mode). Process poll error after a stop → `SANDBOX_STOPPED`, exit 1.
- `cmd_exec.rs`, `acp/sandbox.rs` (`Session` cap in `AgentProcess` next to
  `mcp`; never write to stdout), `cmd_network.rs` (`locate_running`).
- Tests: extend `test_sandbox_client_lookup.rs`
  (`stopped_sandbox_with_content_is_located`,
  `sandbox_without_last_run_config_is_not_startable`); update texts in
  `tests/cli/exec.bats`, `tests/cli/acp.bats`.
- Extra gate: `mise run bats:cli`.

### Phase 5 – start and stop
- `cmd_start.rs`: early check: `project::is_running` → `status`: stopping →
  wait for lock free; sessions or booting (lock held, no sock) → fail
  `START_ALREADY_RUNNING` (names `airlock stop`). Lock busy in
  `ensure_sandbox` → same message. Prep as today (wizard, approvals,
  `ensure_sandbox`, logging attach, `project::open` as env check,
  `install_tools` with its own boot fully down). Drop `Project`,
  `spawn_host(lock)`, then `HostRuntime`, `openSession`, `monitor` if `-m`,
  `run_attached` with `sandbox::main_argv(..)` and empty cwd,
  `terminal.exit`, `notices`, `release`, wait if `last`.
- Pass `--quiet`, `--log-level`, `--verbose`, `--network` to the host.
- New `cli/cmd_stop.rs`, `Command::Stop` (`--follow-git-worktree`): not
  running → `NOT_RUNNING`, exit 0. Else `stop` RPC; lock held but no sock →
  SIGTERM to PID from lock file (retry read while empty). Wait for lock free
  (`SHUTDOWN_TIMEOUT` + 10 s) → `stopped(project)`.
- Keep `interactive.rs` until VM bats pass with the host path, then delete it
  and the interim local monitor source.
- New `tests/cli/stop.bats`; help bats (`sandbox-daemon` hidden, `stop` listed).
  New `tests/vm/sessions.bats`: exec auto-starts after `run_vm true` and stops
  after; two parallel execs share one VM (same boot_id); start fails while
  exec runs; `stop` ends a long exec; forward does not hold sandbox; start
  exit code passes through; exec attached during start keeps VM after start
  exits.
- Extra gate: full `mise run bats` (existing VM tests unchanged).

### Phase 6 – docs
- `/asd-ste100` + manual linter: `usage/attaching-to-running-sandbox.md`,
  `usage/managing-sandbox.md` (stop), `usage/starting-sandbox.md`,
  `usage/monitor.md`, `tips/editor-acp.md`, `technical/rpc.md`,
  `technical/project-layout.md`, `technical/container-execution.md`. Module
  docs of `cli_server.rs`, cli.capnp header.
- `docs/log/2026-10-31-detached-sandbox-host.md`.
- Gate: linter, `mise run docs:build`.

## Messages (`messages.rs`)
`cli`: `never_started(dir)` ("run 'airlock start' first"; replaces
`no_running_sandbox` for exec/acp), `STARTING_SANDBOX`,
`WAITING_FOR_SANDBOX`, `SANDBOX_STOPPED`, `NOT_RUNNING`, `stopped(p)`,
`stop_timeout(secs)`. `start`: `START_ALREADY_RUNNING`. `host`:
`IMAGE_MISSING_RUN_START`, `INSTALLS_PENDING_RUN_START`,
`host_exited_early(code)`, `LOCK_FD_MISMATCH`. Remove the "is 'airlock
start' running" hint from `stale_cli_sock`. Reuse `no_last_run_config`.

## Risks
- macOS Apple VM in a detached process (spike first in Phase 3).
- Monitor event loss under load (as today with `Lagged`).
- Host logs after ready only in airlock.log.
- Every exec in a script without a running session boots a VM (no grace,
  by user decision).

## Verification
- Per phase: `mise format`, `mise run lint`, `mise run test`.
- Phases 4–5: `mise run bats:cli`, full `mise run bats`.
- Manual in `.tmp/test-autostart`: `mise airlock -- start -- true`; then
  `mise airlock -- x -- uname -a` (auto-start, stops after);
  two terminals `x -- sleep 30` (shared VM); `start` while exec runs
  (fails); `stop`; `start -m` network tab and policy change; `acp` with
  stopped sandbox.
