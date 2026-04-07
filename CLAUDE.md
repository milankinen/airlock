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

Non-interactive commands can be tested directly from the CLI:

```bash
# Pipe mode (stdin is not a TTY → no PTY, pipe I/O)
echo "hello" | target/debug/ez -- cat
echo "data" | target/debug/ez -- grep pattern

# Command mode (stdin is TTY → PTY allocated)
target/debug/ez -- ls /usr
target/debug/ez go -- sh -c 'echo hi; exit 42'

# Interactive shell (no subcommand or explicit go)
target/debug/ez
target/debug/ez go
```

Exit codes propagate. `mise run ez` always builds latest
(including supervisor cross-compile) before running:

```bash
mise run ez               # interactive shell
mise run ez -- ls /usr    # command with args
```

## Project management CLI

```bash
# Show info for the project in the current directory
target/debug/ez project info

# List all known projects with abbreviated IDs
target/debug/ez project list

# Remove project by current directory, path, full hash, or abbreviated hash
target/debug/ez project remove
target/debug/ez project remove /path/to/project
target/debug/ez project remove abc1234   # abbreviated hash
target/debug/ez project remove --yes     # skip confirmation prompt
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
