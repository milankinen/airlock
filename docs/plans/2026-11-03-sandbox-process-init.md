# Sandbox process: thin clients, start work in one process

> Note (2026-11-04): this code is now in the `sandbox/` module (see
> `docs/log/2026-11-04-sandbox-module.md`). Old names: `sandbox_process/` →
> `sandbox/process/` (`mod.rs` → `child.rs`), `cli_server.rs` →
> `sandbox/process/server/`, `cli/sandbox_client.rs` and `cli/attach.rs` →
> `sandbox/process/{client,attach}.rs` and the `SandboxProcess` handle,
> `start/` → `sandbox/start/` (`sandbox.rs` → `ensure.rs`, `run.rs` → the
> handle and `cli/cmd_start.rs`), `project/` → `sandbox/{load,lock,meta,disk,env}.rs`,
> `sandboxes/` → `sandbox/{lookup,registry,migrate,first_start,remove}.rs`,
> `ensure_sandbox_process_with` → `client::ensure_with`,
> `start_sandbox_process_with` → `client::start_with`,
> `SandboxProcessLauncher` → `client::Launcher`, `LocatedSandbox` →
> `client::Located`.

Supersedes the launcher, handoff and session parts of
`2026-10-31-detached-sandbox-host.md`. The monitor RPC, approvals,
`run_attached`, sandbox lookup and stop-at-zero rules of that plan stay.

## Context

The branch has a detached daemon that owns the VM, but the clients still
do much work: `start` does all prep and prompts and hands the lock and
the vault passphrase to the daemon, and `exec`/`acp` have a polling retry
loop. Goal: the commands are thin clients. One sandbox process does all
start work (config, prompts, installs, boot, VM lifetime) on the parent's
stdio. When the init is done, it detaches and the parent talks to it only
over `cli.sock`.

`airlockd` already starts no user program at boot. The main command of
`start` runs through the `exec` RPC.

## 1. Names

- Hidden command: `airlock internal sandbox-process <init-fd>`
  (`internal` is a hidden subcommand group for later commands).
- Module `host/` → `sandbox_process/`. Texts say "sandbox process".
  The guest `[daemons.*]` keep their name.
- `ensure_sandbox_process(..) -> SandboxProcess`,
  `sandbox_process.start_session() -> SandboxSession` (one connection),
  test seam `SandboxProcessLauncher`.

## 2. Locks

- `<sandbox>/run.lock`: the sandbox process holds it from its start to
  its exit. The file holds its PID. Held = a sandbox process is alive
  (boots, runs or stops).
- `<sandbox>/startup.lock`: one starter at a time. The parent holds it
  while it decides to spawn. The sandbox process holds it during init and
  boot (also the `start` prompts) and frees it at `Detach`, after its
  parent connected. Init failure or exit frees it too.
- The parent takes both locks, passes both fds (CLOEXEC cleared) and
  closes its copies (no `LOCK_UN`). The child checks each fd against its
  file (`SandboxLock::adopt`: dev/ino, CLOEXEC, PID write for run.lock).
  A crash frees both (kernel).
- Waits are blocking `flock` in a blocking thread, with a Ctrl+C check
  and a timer that shows "waiting for sandbox" after 1 s. No polling.
- The old `<sandbox>/lock` file is not used any more. `rm` needs both
  locks free.

## 3. Init protocol

Unix socketpair; the child end (CLOEXEC cleared) is `<init-fd>`.
Newline-delimited JSON (`Init` line max 1 MiB, other lines max 64 KiB).
A second socketpair carries the parent's RPC connection: the child end
is `rpc_fd`.

```rust
struct Init {
    sandbox_id: String,
    run_lock_fd: i32,
    startup_lock_fd: i32,
    rpc_fd: i32,               // first cli_service connection (parent)
    log: LogOpts,              // --log-level, --verbose, --quiet
    mode: Mode,
}
enum Mode { Start { args: StartArgs }, Exec, Acp } // args: cmd args, login, network
```

1. Parent → child: `Init`.
2. Child does the init for its mode (section 4). After the boot it
   serves `cli_service` on `rpc_fd` as the first counted connection
   (count 1), before it binds `cli.sock`.
3. Child → parent: `Ready { main_argv: Option<Vec<String>> }` (`start`
   only: from args, `-l` and the image cmd).
4. Parent attaches to `cli_service` on its `rpc_fd` end (no `cli.sock`
   connect), then sends `Detach`.
5. Child frees `startup.lock`, points fds 0-2 at `/dev/null`,
   `setsid()`, closes the init fd. Parent sees EOF.
6. Parent starts the program only after this EOF, so the outputs never
   mix.

Errors:
- Init failure: the child prints the error on its stderr (still holding
  `startup.lock`) and exits with
  today's `start` codes (130 abort/interrupt, 2 config, ...). Parent sees
  EOF with no `Ready`, waits for the child and exits with its code.
- Parent dies before `Detach`: EOF on the init fd → the child stops the
  init (or the VM) and exits.
- Signals during init: same process group. Ctrl+C reaches both. The
  parent ignores it and does not touch the terminal (no raw mode, no
  TUI, no stdin read) until EOF after `Detach`. After that EOF it clears
  its interrupt flag (the Ctrl+C was for the child). SIGTERM goes on to
  the child and stays in the flag.

## 4. Sandbox process modes

- `start` (inherited stdio, prompts allowed; without a terminal the
  same rules as today's `start`): `adopt_local_config`, legacy
  `.airlock/sandbox` import into its own locked dir (question), wizard,
  pack approval, config diff, image/tool questions, `ensure_sandbox`,
  save of `last_run_config`, installs, vault, `project::open`, boot with
  the one-run `--network`. If the sandbox had no content at init (first
  start) and the init fails, it removes the registration and the dir
  before exit.
- `exec` (inherited stdio): config only from `last_run_config`; no
  config prompts; vault prompt allowed on a terminal. Errors say "run
  'airlock start'": no last config, missing image/disk, pending installs,
  refused pack approval.
- `acp`: as `exec`, non-interactive: stdin `/dev/null`, stdout and
  stderr are pipes to the parent. Any prompt fails at once (vault only
  with `AIRLOCK_VAULT_PASSPHRASE`).

After `Ready`, all modes: serve `cli.sock`; stop at once when the
connection count goes to 0, on the `stop` RPC, or on SIGTERM. First
connection limit 5 s. Logs only to `<sandbox>/airlock.log` after
`Detach`. Approvals: desktop dialogs in the sandbox process, monitor
dialog over RPC with `start -m`.

## 5. Connection counting

- The server counts connections at `accept`. `openSession`, `Session`
  and `release` RPCs go away.
- At count 0 the server sets "stopping" synchronously and closes new
  connections at once.
- `unref() -> (stopping :Bool)` removes the connection from the count.
  `stopping` is decided synchronously on the server: true when the
  count became 0 (or a stop was requested).
- `network forward` calls `unref` after it connects. Its connection
  then does not count. It closes at shutdown.
- `status` and `stop` probes count while they are open. The parent's
  connection is always first: it comes over `rpc_fd` and counts before
  `cli.sock` exists. So a fast-path probe of another client can never
  bring the count from 1 to 0 before the parent attached.

## 6. Clients

`ensure_sandbox_process(mode, sandbox) -> SandboxProcess`:

exec / acp:
1. Lookup (nearest sandbox with a socket, or with content and
   `last_run_config`). None → "run 'airlock start' first", no spawn.
2. Connect. Works → done.
3. Take `startup.lock` (blocking; waits for a boot).
4. Connect. Works → close `startup.lock`, done.
5. Take `run.lock` (blocking; waits for a stopping process).
6. Spawn with both locks → `Ready` → connect → `Detach` → EOF.
7. `run_attached`. acp keeps the connection for the agent's life.

acp spawn: pipes for stdout/stderr, read into a 64 KiB ring buffer and
copied to `acp.log`. On EOF without `Ready`, reply to `session/new` with
a JSON-RPC error: `message` = last error line, `data` = `{ exitCode,
output }`.

start:
1. Settings, monitor keys, runtime check (fail early).
2. `resolve_sandbox`; else `check_local_config` + `find_or_register`
   (no prompts). Keep `SANDBOX_REMOVED_DURING_START`.
3. Take `startup.lock` (blocking).
4. Connect works and `status` not stopping → "already running" (names
   `airlock stop`).
5. Take `run.lock` (blocking; waits for a stopping process).
6. Spawn `Start { args }` → `Ready { main_argv }` → attach on `rpc_fd`
   → monitor if `-m` → `Detach` → EOF.
7. `run_attached(main_argv)`, print notices.
8. `unref` on its own connection: `stopping` → wait on `run.lock`;
   print an error if the sandbox process exit code says the stop
   failed. Not stopping → close the connection and return.

stop:
1. Connect works → `stop` RPC → wait on `run.lock` → "stopped".
2. Else take `startup.lock` (blocking), then step 1 again.
3. Still no connect while holding `startup.lock` → try `run.lock`
   without blocking: free → "not running"; busy → wait for it.

`network forward`: unchanged lookup (running only), `unref`, no start.

## 7. Stays / goes

Stays: stop at count 0, `cli.sock` RPCs (minus session RPCs), monitor and
approvals over RPC, `run_attached`, lookup rules, `SandboxLock::adopt`,
`sandbox/hosted.rs`, `sandbox/session.rs`, most tests.

Goes: vault passphrase handoff (`handoff_passphrase`, host
`FixedPassphrase` source), stderr copy and `ready` line on stdout,
`write_holder_pid`, SIGTERM-by-PID in `stop`, `start/running.rs`,
polling retry loop and `MAX_LAUNCHES`, client-side `start` prep,
`sandbox-daemon` name, session RPCs.

## 8. Phases

Each: opus producer, sonnet critic, gates (`mise format`, `mise run
lint`, `mise run test`, `mise run bats:cli`), WIP commit.

1. Foundation: rename to `sandbox_process`; `run.lock` / `startup.lock`
   with blocking interruptible acquire and `adopt`; socketpair helper
   in `util/`; `Init`/`Ready`/`Detach` messages; `internal
   sandbox-process` command skeleton (init read, ready, detach, setsid,
   abort on parent EOF); connection counting, `unref`, session RPCs
   removed. Tests: protocol and lock handoff e2e with a test-double
   child; connection count and `unref`.
2. Start work into the sandbox process: `start` mode (prep moved from
   `cmd_start.rs` / `start/run.rs`, legacy import into the locked dir,
   `adopt_local_config`, first-start cleanup), `exec` mode (last-run
   checks), `acp` mode (non-interactive), `main_argv` in `Ready`. Tests:
   mode checks, acp prompt fails, first-start cleanup.
3. Thin clients: `ensure_sandbox_process`, `start_session`, `start`,
   `exec`, `acp` (capture + JSON-RPC error), `stop`, `rm`; remove old
   code. Tests (fake sandbox process): two parallel exec → one spawn;
   exec during boot waits; exec during stop waits on run.lock; start
   during stop waits; start while running fails; stop during boot,
   running, none; acp failure carries captured output.
4. VM bats: update `tests/vm/sessions.bats` and helpers. User tests on
   macOS and Linux with KVM.
5. Later: manual pages, dev log, old plan file update.

## 9. Risks

- Shared terminal during init: the parent must not touch it before EOF.
- Ctrl+Z during init stops both. Accepted.
- Blocking `flock` thread on Ctrl+C: the process exits; harmless.
- A Ctrl+C at the moment of the detach can stay in the interrupt flag of
  the parent. The main command then sees an interrupt. Accepted.
- macOS: `setsid` with the Apple VM and desktop dialogs. User tests.
- Old airlock binaries do not see the new lock files.

## Verification

Per phase gates. Phase 4: `mise run bats` on KVM and macOS. Manual in
`.tmp/test-sandbox-process`: `start -- true`; `x -- uname -a` (start
and stop); two parallel `x -- sleep 20` (one VM); `start` while they run
(fails); `stop` during boot and while running; `start -m` network tab,
policy, approvals; Ctrl+C in a `start` prompt and during boot (130, no
process left); `acp` with locked vault (error with output).
