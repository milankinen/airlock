# Forward host signals to VM + supervisor tracing via RPC

### What

Forward signals received by the CLI (SIGHUP, SIGINT, SIGQUIT, SIGTERM,
SIGUSR1, SIGUSR2) to the container process in the VM. Route supervisor
tracing output through the LogSink RPC so it appears in the CLI.

### Signal forwarding

Signals are captured on the CLI via `async_stream` + `tokio::select!`
over all forwardable signal kinds, yielding Linux signal numbers
(hardcoded constants, not `libc::` — the target is always the Linux VM
regardless of the macOS host where SIGUSR1/2 differ).

The `Process.signal(signum :Int32)` RPC delivers signals to the
supervisor. For SIGINT and SIGQUIT, the supervisor writes the
corresponding PTY control character (`\x03`, `\x1c`) to the PTY
master — this goes through the terminal discipline and works across
PID namespaces, exactly like Ctrl+C. For other signals, it falls
back to `kill(-pid, sig)` on crun's process group.

The PTY control character approach was chosen because:

- crun runs the container in a PID namespace
- The shell has job control disabled ("can't access tty")
- `tcgetpgrp()` returns crun's PGID, not the shell's
- `kill()` to crun's process group doesn't reach container processes
- But writing `\x03` to the PTY master works (same as Ctrl+C)

### Supervisor tracing via LogSink RPC

Added a tracing subscriber layer to the supervisor that forwards all
`tracing::*!()` events through the LogSink RPC to the CLI. The CLI's
LogSinkImpl now uses `tracing::debug!/info!/warn!/error!` with
`target: "vm"` so supervisor messages go through the CLI's tracing
subscriber and respect `--verbose`.

Removed the manual `Logger` struct from the network proxy in favor of
standard tracing macros.
