# Attaching to a running sandbox

Once a sandbox is running (started with `airlock start`), you can open
additional sessions into it using `airlock exec`. This is useful when you
need a second terminal inside the VM — for example, to run tests in one
window while an agent works in another.

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
`airlock exec` looks for a running sandbox in the current directory, then
in each parent directory. The nearest running sandbox wins. The command
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

The forwards stay active while the command runs. Press Ctrl+C to stop them.
The command exits when the sandbox stops. The command fails if a port is in
use or conflicts with a configured forward.

Ad-hoc forwards follow the same network policy as
[configured port forwards](../configuration/network.md#port-forwarding).
