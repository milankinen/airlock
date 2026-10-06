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

## Temporary files

IMPORTANT: Write temporary files **ALWAYS** to this project's `.tmp`
directory instead of `/tmp`. Delete temporary files immediately
after their use unless told otherwise.

## Manual testing

Test airlock changes with `mise airlock` from a test directory under
`.tmp` (e.g. `.tmp/test-<name>`, with its own `airlock.toml` if needed).
The task builds the dev binary and runs it in the current directory.
mise consumes the first `--`, so put one before the airlock arguments:

```bash
mkdir -p .tmp/test-foo && cd .tmp/test-foo
mise airlock -- start -- echo hello
```

## User manual

ALWAYS use `/asd-ste100` skill (STE-flavored mode) when editing manual
pages. `intro.md` Motivation section exempt. Lint changed pages using
`mise x -- python <linter-script-and-args>`.

False positives: CSS/HTML in inline SVGs, "tl;dr"/"effortless" in
Motivation. Passive-voice and tense findings are advisory.

## Development Log

Log entries live in `docs/log/` as individual files named
`<yyyy-mm-dd>-<title>.md` (one entry per file). When adding a new
log entry, create a new file there — do NOT append to a combined log.

## Commits

ALWAYS use `/git-commit` skill when doing git commits and ALWAYS
follow skill instructions and steps!
