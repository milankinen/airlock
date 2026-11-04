# Running commands in sandbox

Once a sandbox exists, you can run commands in it with `airlock exec`.

All `airlock exec` commands share the same sandbox, and airlock handles
its lifecycle for you. If the sandbox is not running, the first command
starts it, and the next commands use the running sandbox. The sandbox
stops when the last command exits.

`airlock exec` never asks questions and never applies config changes to
the sandbox. When you change the config, apply the changes first with
`airlock start`, so that `airlock exec` sees them.

```bash
airlock exec bash
```

The shorthand alias `airlock x` does the same thing:

```bash
airlock x bash
```

You can run any command, not just shells:

```bash
airlock exec cat /etc/os-release
airlock exec python3 -m pytest tests/
```

## Working directory

You can use `airlock exec` in any subdirectory of the project.
`airlock exec` looks for a sandbox in the current directory, then in each
parent directory, and uses the nearest one, even if it is not running.
A stopped sandbox starts with the config of the last `airlock start`.
If the sandbox was never started, the command fails. The command
runs in the same working directory as on the host. If the sandbox
started with `--sandbox-cwd`, the command runs in the matching directory
below that guest path. To use a different directory, use `--cwd` (or `-w`):

```bash
# /home/example/my-project
#   airlock.toml
#   src 

cd src
airlock x pwd          # prints: /home/example/my-project/src
airlock x -w /tmp pwd  # prints: /tmp
```

## Git worktrees

airlock supports git worktrees that live outside their main repository.
To use the sandbox of the main repository from such a worktree, use
`--follow-git-worktree`:

```bash
cd /home/example/worktrees/my-feature
airlock x --follow-git-worktree git status
```

> [!IMPORTANT]
> airlock mounts only the project directory automatically. Add a
> [mount](../configuration/mounts.md) for the worktrees directory, so that
> the worktrees are available in the sandbox.

## Environment variables

Pass extra environment variables with `-e` (repeatable):

```bash
airlock exec -e DEBUG=1 -e LOG_LEVEL=trace ./run-tests.sh
```

airlock layers them on top of the sandbox's resolved environment
(image env + `airlock.toml` env). Entries with the same key replace
the base value.

## Login shell

Like `start`, the `--login` flag sources profile scripts before running the
command:

```bash
airlock exec --login bash
```

## Port forwarding

Use `airlock network forward` to open ports between the host and a running
sandbox without a config change. Each forward is `8080` (same port on both
sides) or `host:guest`. The left side is always the host port.

* Default (`--mode=guest`): publish sandbox ports on the host.
* `--mode=host`: let the sandbox reach host ports.

```bash
# Open http://localhost:8000 on the host
airlock network forward 8000

# Sandbox port 3000 reaches host port 9000
airlock network forward --mode=host 9000:3000
```

The forwards stay active while the command runs. Press Ctrl+C to stop
them. Forwards do not keep the sandbox running. The command exits when
the sandbox stops, for example when the last `start`, `exec` or `acp`
client ends. It fails if a port is in use or conflicts with a configured
forward.

Ad-hoc forwards follow the same network policy as
[configured port forwards](../configuration/network.md#port-forwarding).
