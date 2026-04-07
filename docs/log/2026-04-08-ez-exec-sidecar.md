# ez exec — sidecar process attachment

Adds `ez exec` (alias `ez x`) for attaching new processes to a running
container without a TTY handoff dance.

## Architecture

`ez go` now spawns a Cap'n Proto RPC server on `<project-cache>/cli.sock`
after the VM boots. `ez exec` connects to this socket as a client,
bootstraps `CliService`, and sends an `exec` request carrying:

- a `Stdin` capability (relaying terminal input back over the unix socket)
- PTY size (if stdin is a TTY)
- command, args, cwd, env

The CLI server (`cli_server.rs`) bridges the two RPC connections:

- **StdinBridge**: implements `stdin::Server` on the vsock side, forwarding
  each `read()` call to the unix-socket `Stdin` client from `ez exec`
- **ProcessBridge**: implements `process::Server` on the unix-socket side,
  forwarding `poll()`/`signal()`/`kill()` to the vsock `Process`

This keeps Cap'n Proto capabilities on their native connection while
bridging I/O across the unix→vsock boundary.

## crun exec construction at CLI level

`build_exec_command()` in `rpc/supervisor.rs` constructs the full
`crun exec [--tty] [--cwd …] [--env …] ezpez0 cmd args` invocation,
mirroring how `build_command()` constructs `crun run` for `ez go`. The
supervisor's `exec` handler then just calls the generic `spawn()` — no
`spawn_exec` wrapper needed. This also means dev mode (`EZ_DEV_NO_CRUN`)
can bypass `crun` for exec the same way it does for run.

`Supervisor.exec` in the schema takes only `stdin`, `pty`, `cmd`, `args` —
cwd/env are baked into args by the CLI before the RPC call. `CliService.exec`
retains cwd/env as separate fields since it's the user-facing interface.

## Default cwd

When `-w` is not given, `ez exec` defaults to `project.cwd` (the host
project directory). This works because virtiofsd mounts the project at the
same absolute path inside the container — identical to the `cwd` set in the
OCI config for `ez go`.
