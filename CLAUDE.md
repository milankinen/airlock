Lightweight VM based sandbox for easy untrusted code execution.

## Bash commands and mise

When executing bash commands inside this project, always use
mise to get project tooling and environment variables:

```bash
mise x -- <cmd> <args...>
```

For common operations, add mise tasks proactively and prefer to
use them always instead of raw commands:

```bash
mise run <task>    # Run task
mise tasks --all   # List available tasks
```

## Formatting

Always format code you produce. Use `mise format`

* Do NOT use `cargo fmt` directly because it uses wrong `rustfmt` version).

## Testing the VM

IMPORTANT: if `NO_KVM` environment variable is set to `1`, it means
that there is no KVM support and running `airlock go` will fail.

Non-interactive commands can be tested directly from the CLI:

```bash
# Pipe mode (stdin is not a TTY → no PTY, pipe I/O)
echo "hello" | target/debug/airlock go -- cat
echo "data" | target/debug/airlock go -- grep pattern

# Command mode (stdin is TTY → PTY allocated)
target/debug/airlock go -- ls /usr
target/debug/airlock go -- sh -c 'echo hi; exit 42'

# Interactive shell (no subcommand)
target/debug/airlock go
```

Exit codes propagate. `mise run airlock` always builds latest
(including supervisor cross-compile) before running:

```bash
mise run airlock               # interactive shell
mise run airlock -- ls /usr    # command with args
```

## Exec (sidecar) CLI

With a VM running (`airlock go`), attach additional processes in a separate terminal:

```bash
# Run a command inside the running container (alias: airlock x)
target/debug/airlock exec ls /usr
target/debug/airlock exec sh -c 'echo hi'
target/debug/airlock x bash

# With options
target/debug/airlock exec -w /app bash          # set working directory
target/debug/airlock exec -e KEY=val env        # set env vars
```

`airlock exec` connects to `<project-cache>/cli.sock` (Cap'n Proto RPC) that
`airlock go` exposes while the VM is running. TTY mode is auto-detected; raw
mode is enabled for interactive commands. The `CliService` interface is
defined in `crates/common/schema/supervisor.capnp`.

## Project management CLI

```bash
# Show info for the project in the current directory
target/debug/airlock project info

# List all known projects with abbreviated IDs
target/debug/airlock project list

# Remove project by current directory, path, full hash, or abbreviated hash
target/debug/airlock project remove
target/debug/airlock project remove /path/to/project
target/debug/airlock project remove abc1234   # abbreviated hash
target/debug/airlock project remove --yes     # skip confirmation prompt
```

## Temporary files

IMPORTANT: Write temporary files **ALWAYS** to this project's `.tmp`
directory instead of `/tmp`. Delete temporary files immediately
after their use unless told otherwise.

## Development Log

Log entries live in `docs/log/` as individual files named
`<yyyy-mm-dd>-<title>.md` (one entry per file). When adding a new
log entry, create a new file there — do NOT append to a combined log.

## Commits

ALWAYS use `/git-commit` skill when doing git commits and ALWAYS
follow skill instructions and steps!
