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
target/debug/ez -- sh -c 'echo hi; exit 42'

# Interactive shell (no args)
target/debug/ez
```

Exit codes propagate. `mise run ez` always builds latest
(including supervisor cross-compile) before running:

```bash
mise run ez               # interactive shell
mise run ez -- ls /usr    # command with args
```

## Temporary files

IMPORTANT: Write temporary files **ALWAYS** to this project's `.tmp`
directory instead of `/tmp`. Delete temporary files immediately
after their use unless told otherwise.

## Commits

ALWAYS use `/git-commit` skill when doing git commits and ALWAYS
follow skill instructions and steps!
