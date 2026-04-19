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

`airlock exec` walks up from the current directory to find the
sandbox's `.airlock/sandbox/cli.sock` and runs the command in the
same directory inside the VM. To override, use `--cwd` (or `-w`):

```bash
airlock exec -w /tmp ls -la
```

## Environment variables

Extra environment variables can be passed with `-e` (repeatable):

```bash
airlock exec -e DEBUG=1 -e LOG_LEVEL=trace ./run-tests.sh
```

They're layered on top of the sandbox's resolved environment (image
env + `airlock.toml` env); entries with the same key replace the
base value.

## Login shell

Like `start`, the `--login` flag sources profile scripts before running the
command:

```bash
airlock exec --login bash
```

