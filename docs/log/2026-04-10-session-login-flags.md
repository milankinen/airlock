# Add --session and --login flags to ez go/exec

## --session

Named sessions let multiple independent runs coexist for the same project
directory. Without a session, `ez go` uses `~/.ezpez/projects/<hash>/` as
before. With `--session=foo`, it uses `~/.ezpez/projects/<hash>.foo/` —
completely isolated overlay, CA, lock, cache.

`ez project list` displays them as `<abbrev>:<session>` (e.g. `abc1234:foo`).
The `.` separator is used on disk to keep the directory name a single token;
`:` is used for display/CLI since it's more readable and avoids confusion with
file extensions.

`min_unique_prefix_len` was updated to deduplicate by hash part so that
sessions of the same project share their abbreviation length.

`ez project info` and `ez project remove` accept `<hash>:<session>` identifiers
via the existing path/hash argument.

The `mise run vibe <feature>` task was updated to use `--session=<feature>`
instead of the previous worktree-based approach.

## --login / -l

Wraps the container command in a bash login shell so `/etc/profile`,
`~/.bash_profile`, and `~/.bashrc` are sourced (e.g. mise shim activation,
custom PATH entries).

Two cases:
- **Lone shell binary** (`ez go --login`, `ez exec --login bash`): adds `-l`
  directly to the shell invocation.
- **Any other command** (`ez go --login -- claude`, `ez exec --login node`):
  wraps as `bash -l -c 'exec "$0" "$@"' cmd args...`. The `$0`/`$@` trick
  avoids any quoting — args are passed as positional parameters directly.

`bash` is used (not `sh`) because mise activation lives in `~/.bashrc` which
only bash login shells source (via `~/.bash_profile` → `~/.bashrc`). Plain
`sh -l` only reads `~/.profile`, missing mise's PATH setup.

`dev.dockerfile` was updated to wire `~/.bash_profile` → `~/.bashrc`, and
the `~/.bashrc` PATH line was fixed (missing `~/` prefix).

## Supervisor init error output

The fallback process on init failure (`exit 100`) now prints the error message
to stderr before exiting, using `printf '%s\n' 'error: ...' >&2`. Single quotes
in the message are escaped with the `'\''` trick. Using `{e}` (not `{e:#}`)
avoids duplicate messages when the outer anyhow error and its io::Error source
have the same display string.
