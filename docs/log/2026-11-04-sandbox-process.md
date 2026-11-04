# Sandbox process: auto start and shared sandbox

## Goal and user-visible behavior

Before, `airlock start` was the only way to boot a sandbox, and it held
the VM for its own life. Now one sandbox process owns the VM. Every
client command is a thin client of that process:

* `airlock start` starts the sandbox, runs the main command and exits
  with its exit code. It fails if the sandbox already runs.
* `airlock exec` and `airlock acp` start the sandbox with the config of
  the last `start` if it does not run. They connect if it runs. Two
  parallel `exec` commands share one VM.
* The sandbox stops when the last connection closes. `airlock stop`
  stops it at once.
* The passphrase prompt of the vault comes one time.
* Without a last `start`, `exec` and `acp` fail with "the sandbox of
  <project> never started. run 'airlock start' first". They find the
  nearest registered sandbox and take no running parent sandbox.

## Sandbox process

The sandbox process is the hidden command `airlock internal
sandbox-process <init-fd>`. A client starts it and gives it the held
locks. It owns the VM, the network and the approvals of the sandbox. It
does all start work on the terminal of its parent: the questions, the
installs and the boot. Thus the clients need no copy of the start logic,
and no vault passphrase goes between processes. The earlier passphrase
handoff is gone.

Desktop dialogs run in the sandbox process. It takes the environment of
the process that started it (DISPLAY, WAYLAND_DISPLAY, D-Bus address).
After the detach it writes only to `<sandbox>/airlock.log`.

## Locks

Two lock files in the sandbox directory:

* `run.lock`: the sandbox process holds it from its start to its exit.
  A held lock means a sandbox process is alive (boots, runs or stops).
  The file holds its PID, for diagnostics only. The sandbox process
  writes it at adopt. A parent that takes the lock empties the file, so
  nobody reads a stale PID.
* `startup.lock`: one starter at a time. The sandbox process holds it
  from the spawn until the detach, also during the `start` questions.

Why two: one lock cannot tell "a process boots" from "a process runs".
A client needs both. It waits on `startup.lock` for a boot to end, then
connects. It waits on `run.lock` for a stopping process to exit, then
starts a new one.

Handoff: the parent takes both locks, clears CLOEXEC, passes the
descriptors and closes its copies. The child checks each descriptor
against its file (`SandboxLock::adopt`: dev/ino and CLOEXEC). The kernel
frees both locks on a crash.

Waits are a blocking `flock` in a plain thread, not `spawn_blocking`:
the runtime waits for blocking tasks at exit. A wait stops on Ctrl+C and
tells "waiting for the sandbox" after 1 s. There is no polling loop.
Probes of a lock take a free lock for a moment, so `start` retries its
non-blocking takes for a short time.

The old `lock` file is not used. Only the legacy `.airlock/sandbox`
import still checks it, for older binaries.

## Init handshake

The parent and the child talk over a socket pair, in newline JSON.
`Init` line max 1 MiB, `Ready` 2 MiB, other lines 64 KiB.

1. Parent sends `Init`: sandbox id, lock descriptors, `rpc_fd`, log
   options and the mode (`Start { args }`, `Exec`, `Acp`).
2. The child does the init for its mode. Then it serves the CLI service
   on `rpc_fd` as the first counted connection, before it binds
   `cli.sock`. Without this, a fast `exec`, `stop` or `network forward`
   could be the first connection and stop the sandbox when it closes.
3. Child sends `Ready { main_argv }`. `main_argv` is set for `start`
   only.
4. The parent attaches to the CLI service on its `rpc_fd` end. `start
   -m` subscribes the monitor here, so no approval request of the boot
   comes before the TUI.
5. Parent sends `Detach`. The child frees `startup.lock`, points fds 0-2
   at `/dev/null`, calls `setsid`, sends `Detached` and closes the
   socket.
6. The parent starts the main command only after this EOF, so outputs
   never mix.

Errors and exit codes:

* The child prints an init error on its stderr, still holding
  `startup.lock`, and exits with the `start` codes (130 abort or
  interrupt, 2 config). EOF without `Ready` gives `ExitedBeforeReady
  { code, output }`, and the client exits with that code.
* EOF without `Detached` (for example Ctrl+C at the detach) also gives
  `ExitedBeforeReady`. A failed connect step waits 500 ms for the exit
  and then gives its code. A message is more reliable than `try_wait`: a
  process can close its descriptors before the kernel marks it exited.
  Thus `exec` and `start` exit with 130, not with an RPC error.
* Parent death before `Detach`: a watch thread with its own runtime sees
  the EOF and sets the interrupt flag. A question ends at its next poll
  (20 ms), the init fails with 130 and the first start cleanup runs. The
  thread also sees SIGTERM and SIGHUP.
* Signals: Ctrl+C reaches both processes. The parent ignores SIGINT
  during the init and does not touch the terminal. After the detach it
  clears its interrupt flag, because the Ctrl+C was for the child. It
  then reads pending SIGTERMs without a wait and sets the flag again for
  one. SIGTERM goes on to the child.
* Stdio: the child inherits the stdio of the parent. After the detach it
  uses `/dev/null`, so the VM helpers keep no terminal.

## Modes

* `start`: all start work, in the sandbox process, under the adopted
  locks. The fill of a new sandbox (legacy import or local config move),
  the wizard, pack approval, config diff, image and tool questions,
  `ensure_sandbox`, installs, `open` and the boot with the one-run
  `--network`. `Init` carries the `start` flags the child needs. `-m`
  stays in the client. The guest working directory is the project
  directory unless `--sandbox-cwd` is set.
* `exec`: the last-run checks only, no config questions. The vault can
  ask on a terminal. Errors say "run 'airlock start'": no last config,
  missing image or disk, pending installs, refused pack approval.
* `acp`: as `exec`, non-interactive. The sandbox process sets the
  interactive flag to false, stdin is `/dev/null`, and stdout and stderr
  are pipes to the parent. Each prompt fails at once. The vault works
  only with `AIRLOCK_VAULT_PASSPHRASE`. The client copies the pipes line
  by line to `acp.log` and keeps the last 64 KiB. The pipes end at the
  detach, and the capture waits up to 1 s for the last lines. On EOF
  without `Ready`, `session/new` gets a JSON-RPC error: `message` = last
  stderr line, `data` = `{ exitCode, output }`.

First start cleanup: a directory with only the lock files and the log is
a new sandbox. If its init fails or aborts before the sandbox has a
disk, the sandbox process removes the registry entry and the directory,
with the locks held. A sandbox with a disk stays: a failed install is
retried at the next start without a question. A sandbox with user files
stays: a legacy import that worked, an import whose disk could not go
back (`DiskKept`), and a local config move, also a failed one.

Legacy import and local config move run after the wizard and the config
resolve. A config error or an abort leaves `.airlock` as it is. Until
the move, a new sandbox loads its local config from `<project>/.airlock`.
`import_legacy` imports into the given locked directory. It does not
register or lock. Its rollback moves only the disk back.

## Connection counting

* Each accepted connection holds the sandbox. The sandbox stops when the
  count goes to 0 after at least one connection, on the `stop` RPC, on
  SIGTERM, or when no first connection comes in 5 s.
* `unref() -> (stopping)` drops the hold and keeps the connection. `network
  forward` calls it after it connects, so a port forward does not keep
  the sandbox alive. `stopping` is true if the sandbox stops now.
* The tracker sets "stopping" in the same step as the count change.
  After that, the server closes new connections at once. Clients probe
  with `status` (connection count and `stopping`) and then wait for the
  stop.
* `start` calls `release` after its main command. If the sandbox stops,
  it waits on `run.lock`, so a new `start` does not meet a half stopped
  sandbox. If other connections hold the sandbox, it just exits.
* `status` and `stop` probes count while they are open. The first
  connection of the parent comes over `rpc_fd`, so a probe can never
  bring the count from 1 to 0 before the parent attached.

## Clients

All commands: parse, `find` the sandbox, `run` or `connect` or a
`Sandbox` method, process calls, exit code.

* `exec`, `acp` (`Sandbox::run`): check content and last config, else
  the "never started" error. Connect. If this fails: blocking
  `startup.lock`, connect again, blocking `run.lock`, spawn with both
  locks, `Ready`, attach, `Detach`. `acp` keeps its connection for the
  life of the agent.
* `start`: find or register the sandbox (no prompts, no lock), start
  checks and runtime (`[monitor.keys]`), blocking `startup.lock`, the
  registry check (`SANDBOX_REMOVED_DURING_START`), connect probe ("already
  running", names `airlock stop`), blocking `run.lock`, spawn. `start`
  waits for a boot of another process and then fails as running.
* `stop` (`Sandbox::stop`): `stop` RPC, then wait on `run.lock`. If no
  connect works, blocking `startup.lock` and the RPC again. Still none:
  free `run.lock` is "not running", busy `run.lock` is a stopping sandbox
  to wait for. A `stop` during `start` questions waits until they end.
  The lookup of `stop` also takes a registered sandbox whose lock is
  held, so a `stop` during a first start finds it. No signal by PID.
* `rm`: needs both locks free. It makes a missing lock file, so a new
  `start` waits for the removal.
* `network forward`: running sandbox only, `unref`.
* `Sandbox::connect` checks the symlink of the sandbox dir and
  `cli.sock`. A stale socket gives "no running sandbox".

## Monitor and approvals over RPC

The VM and network run in the sandbox process, the TUI in the client of
`start -m`. `CliService.monitor(sink)` gives a `MonitorSink` and returns
a `MonitorControl` and the policy at the attach.

* Feed: events in batches, stats once per second, policy changes. A
  slow client loses events and never slows the network. The sink call
  also waits (flow control), and the client has small queues for the TUI.
  The feed stops when a sink call fails or the client releases the
  control.
* Approvals: `MonitorSink.approve(request) -> (allow)` carries the clean
  title, body, details with diff lines and `timeLeftMs`. The client puts
  the request into its TUI dialog.
* Dialog list: `MonitorSlot` holds the dialogs of the attached clients.
  The newest live dialog asks. When it detaches or its call fails, the
  next newest asks. A dead dialog is removed when the list is read.
* Deadlines: the sandbox process cancels the call at the approval
  timeout. The client waits for `time_left` (max 1 h) plus 100 ms slack,
  then drops the reply and the TUI removes the closed request. A late
  answer has no effect on both sides.
* A closed dialog or an RPC error denies the request ("the monitor TUI
  stopped before an answer"). With no monitor, `system` mode denies with
  the desktop reason and `monitor` mode with "monitor is not open".

## Module layout

All sandbox code is in `sandbox/` with a small facade. Before it was in
seven places. The low-level `vm/` module (hypervisors, disk, mounts)
stays apart.

```
sandbox/
  mod.rs      facade (re-exports)
  lookup.rs registry.rs lock.rs meta.rs disk.rs load.rs env.rs
  migrate.rs first_start.rs remove.rs
  start/      start work (prepare, wizard, config_change, install,
              check_env, exit, location, ensure)
  process/    mod.rs (Run, SandboxProcess), client.rs, attach.rs,
              forward.rs, launch.rs, init.rs, child.rs,
              server/ (CLI server, connections, monitor), vm/
```

The facade: `find`, `find_or_register`, `get`, `list`, and `Sandbox` with
`run(Run::Exec | Acp | Start)`, `connect` and `stop`. `SandboxProcess`
hides the capnp client: `exec`, `exec_main`, `exec_piped`,
`mcp_registrar`, `notices`, `forward`, `release`, `wait_stopped`.
Submodules are private. The `submodules!` macro makes them `pub(crate)`
in test builds only, so `test_cfg` and other tests reach the internals
with no test re-export list. Tests sit in `sandbox/tests/`,
`sandbox/process/tests/` and `sandbox/start/tests/`.

## Rejected alternatives

* A sessions RPC (`openSession`, `Session`, `release`): connection
  counting at accept is simpler and has no session state to leak.
* A polling retry loop in `exec` and `acp`: the two blocking locks give
  the same result with no timing guesses.
* Passphrase handoff to a detached process: it moves a secret between
  processes. The child opens the vault on the shared terminal instead.
* The lock in the sandbox process only: a gap remains between the check
  and the spawn. The parent takes the locks and passes them on.

## Accepted risks and open items

* A SIGINT in the short time after the child detached and before the
  parent reads `Detached` still goes to the child. It can then stop the
  sandbox that just detached.
* Ctrl+Z during the init stops both processes.
* An approval request that comes after the monitor attach and before
  the TUI starts waits for the TUI dialog, up to its deadline.
* Old binaries do not see the new lock files and the old `lock` file.
* The template build (`templates/create.rs`) uses the start steps
  through re-exports of the sandbox module. A move of the template
  build under the start steps would make the facade smaller.
* macOS is untested: `setsid` with the Apple VM, and the desktop dialogs
  of the detached process.
