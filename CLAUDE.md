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

## Tests

Run Rust tests with `mise run test`, bats tests with `mise run bats`.

What to test:

* Do not test trivial things (constants, getters, derives, plain
  struct construction, one-line wrappers, behavior covered by another
  test).
* Prefer end-to-end tests that run a full sub-system flow with real
  use-case data. Use real local servers and real protocols; fake only
  the edges, e.g. network tests use real upstream servers and replace
  only the sandbox transport.
* Do not test removed features (no negative "X is gone" asserts).

Placement:

* End-to-end tests: one file per flow, `test_<flow>.rs`, in the
  `tests` submodule of the sub-system, e.g.
  `network/tests/test_http2_upstream_connect.rs`.
* Unit tests: inline `mod tests` at the end of the file under test.
* Gate all test code with `#[cfg(test)]`.

Helpers, fixtures and setup:

* Do not duplicate them. Crate-level helpers live in the crate's
  `test_cfg` module (`src/test_cfg.rs`), one `.rs` file per logical
  part (e.g. `test_cfg/network.rs`).
* Helpers that more than one crate needs live in the
  `airlock-test-utils` crate (dev-dependency only, helper groups behind
  features). `test_cfg` re-exports them.

Names: `doing_something_with_some_condition_has_some_effect`, e.g.
`websocket_upgrade_forged_by_middleware_is_refused`. Use simple words
and drop "a"/"the" unless needed.

Comments: do not write comments on tests or in test bodies.

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
