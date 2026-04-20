# Daemons

Daemons are sidecar processes that run in parallel with your main shell.
They are useful for anything that needs to stay alive for the duration of
the sandbox — a database, a language server, `dockerd` inside the VM, or
a build watcher.

Each daemon is declared as `[daemons.<name>]`. Daemons start just after
the VM is ready and before the main shell, and are shut down cleanly
when the main shell exits.

## Minimal example

```toml
[daemons.redis]
command = ["redis-server", "/etc/redis.conf"]
```

That is enough to keep a single Redis server running for the life of the
sandbox. On crash it restarts (up to 10 times by default) with a one
second delay per retry. When you exit the shell, it is sent `SIGTERM`
and given 10 seconds to stop before being `SIGKILL`ed.

## Full reference

```toml
[daemons.my-daemon]
enabled      = true                # default true
command      = ["cmd", "arg1"]     # required; argv-style
cwd          = "/"                 # default "/"
signal       = "SIGTERM"           # default; graceful-stop signal
timeout      = 10                  # default; seconds before SIGKILL
restart      = "always"            # default; or "on-failure"
max_restarts = 10                  # default; 0 = infinite
harden       = true                # default; per-daemon override

[daemons.my-daemon.env]
FOO = "literal"
BAR = "${HOST_VAR}"                # ${VAR} resolved at start
```

### `command`

Argv-style list. The first element is the executable (looked up on
`$PATH` unless absolute), the rest are arguments. Required.

### `cwd`

Working directory inside the sandbox. Defaults to `/`.

### `signal`

The signal used to ask the daemon to shut down gracefully. One of
`SIGTERM`, `SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2`, `SIGKILL`.
Any other name is a config error. Default: `SIGTERM`.

### `timeout`

Seconds to wait after `signal` before escalating to `SIGKILL`. `0` means
wait forever — the `SIGKILL` step is skipped and airlock will block at
shutdown until the daemon exits on its own. Default: `10`.

### `restart`

- `always` (default) — restart whenever the daemon exits, until
  `max_restarts` is reached.
- `on-failure` — restart only on non-zero exit. A clean exit ends the
  restart loop and the daemon is reported as "stopped".

### `max_restarts`

Maximum number of restart attempts after the initial launch. `0` disables
the cap. Default: `10`. Retries use linear backoff (`attempt_number` seconds).

### `harden`

Whether the sandbox hardening (`no_new_privs`, namespace isolation) applies
to this daemon. Per-daemon override of the global `vm.harden` setting —
set to `false` for daemons that need extra privileges (e.g. `dockerd`).
Default: `true`.

### `env`

Per-daemon environment variables. Supports the same `${VAR}` and
`${VAR:default}` substitution as the top-level [`[env]`](./env.md)
section, resolved from the host environment and the
[secret vault](../secrets.md). Values declared here layer on top of
the image's baseline environment; the daemon does not inherit the main
shell's `[env]` entries.

## Logs

Each daemon's stdout and stderr are redirected to files under
`/airlock/daemons/<name>/` inside the VM:

```
/airlock/daemons/<name>/stdout.log
/airlock/daemons/<name>/stderr.log
```

The log files are truncated each time the sandbox starts, and appended to
across automatic restarts within a single session.

## Shutdown

When the main shell exits, airlock asks each daemon to stop (by sending
`signal`), waits up to `timeout` seconds, then escalates to `SIGKILL` for
anything still alive. The CLI shows a spinner per daemon during this
window and prints a final status line:

```
  ✓ daemon redis: shut down
  ✓ daemon dockerd: killed
```

`killed` means the daemon had to be `SIGKILL`ed; `shut down` means it
exited on its own within the timeout.

## Disabling a daemon

A daemon can be disabled without removing the entry from the config —
useful when a preset defines one you don't need:

```toml
[daemons.redis]
enabled = false
```
