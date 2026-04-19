# Container execution

## Process spawning

The supervisor (`airlockd`) does not use an OCI runtime. After
assembling the overlayfs rootfs, it spawns container processes
directly via fork + chroot + exec:

- **chroot** into the assembled overlayfs rootfs.
- **uid/gid** switched to the container user (read from `start` RPC
  params, derived host-side from the image's `/etc/passwd`).
- **PTY** allocated when stdin is a TTY; the host terminal size is
  sent as the initial PTY dimensions, and resize events (SIGWINCH) are
  forwarded.
- **Pipe mode**: when stdin is not a TTY, separate stdout/stderr pipes
  are used with no PTY.

All process configuration (`cmd`, `args`, `env`, `cwd`, `uid`, `gid`)
is carried in the `start` RPC call rather than written to a
`config.json` file.

## stdio over RPC

The container process's stdio doesn't flow through any virtio console
— all three streams are relayed over the same vsock RPC connection
that carries control calls. This means the user sees output as fast as
the supervisor can read it off the PTY/pipe, and the host terminal's
raw-mode keystrokes and resize events reach the container directly.

### Pull-based protocol

Both directions use a pull model:

- **CLI → guest (input + resize)**: the supervisor calls
  `Stdin.read()` on a capability the CLI passed in at `start` time.
  Each frame is either keyboard data (`DataFrame`) or a terminal
  resize (`TermSize`) — multiplexed on the same stream so a resize
  can't race a write to the PTY writer half.
- **guest → CLI (output + exit)**: the CLI calls `Process.poll()` in
  a loop and gets back either `stdout` bytes, `stderr` bytes, or an
  exit code. PTY mode collapses `stdout`/`stderr` into a single
  stream (the PTY has only one output side); pipe mode keeps them
  separate.

Pull-based rather than push-based because vsock latency is negligible
and pull gives natural backpressure — the guest only sends when
somebody is actively reading, and Cap'n Proto RPC handles the
multiplexing of many in-flight calls over the single vsock
connection.

### PTY mode vs pipe mode

The CLI decides mode at startup by checking whether its own stdin is
a TTY:

| CLI stdin  | Container gets                       | Streams           |
|------------|--------------------------------------|-------------------|
| TTY        | A PTY (`/dev/pts/*`) via `pty-process` | stdout only (PTY merges) |
| not a TTY  | Three pipes                          | stdout + stderr separate |

Pipe mode is what makes `echo data \| airlock exec -- grep pattern`
and `airlock -- sh -c 'echo hi; exit 42'` behave like a normal Unix
pipeline — exit codes propagate, stderr doesn't mix into stdout, and
the CLI doesn't put the host terminal into raw mode.

In PTY mode the host terminal is put into raw mode, SIGWINCH is
hooked, and the `Stdin.read()` loop on the CLI side uses
`tokio::select!` to multiplex terminal reads with resize events into
the single stream the supervisor pulls.

### Exit propagation

The supervisor awaits `child.wait()`, encodes the exit code into the
final `Process.poll()` frame, and the CLI calls `std::process::exit`
with that code. When the child is killed by a signal, the signal
number is folded into the exit code using the Unix convention
(`128 + signum`).

## `airlock exec` — attach to a running container

`airlock exec` attaches a new process to an already-running container
without rebooting the VM. The flow:

1. `airlock exec` walks up from the current working directory looking
   for `.airlock/sandbox/cli.sock`. First hit wins — this is how a
   sibling project directory still finds its running VM when invoked
   from a subdirectory.
2. It connects to that socket (Cap'n Proto RPC over a Unix domain
   socket) and calls `CliService.exec(cmd, args, cwd, env)`. `env`
   carries only the `-e KEY=VAL` overrides the user passed on the
   command line; no project load, no vault unlock.
3. The CLI server — running inside the `airlock start` process, next
   to the live `VmInstance` — merges the overrides onto the sandbox's
   resolved base env (image env + `airlock.toml` env resolved once at
   start) and forwards the call to the in-VM supervisor over the
   existing vsock.
4. The supervisor forks a new process inside the container's chroot
   and relays stdio back to the `airlock exec` terminal through a
   bridge that translates between the Unix-socket RPC and the vsock
   RPC.

## Why the CLI-server-side env merge

The CLI server sits in the same process that holds the authoritative
`VmInstance.env`. Doing the merge there means:

- `airlock exec` never loads the project, unlocks the vault, or
  re-resolves anything — it's a tiny socket client.
- The base env is resolved exactly once, at `airlock start`, not once
  per `exec`.
- Processes attached via `exec` inherit the same environment the main
  container process was launched with, which matches the user
  expectation of "start a shell inside my running sandbox with the
  same vars".
