# Claude pack wrapper: WAYLAND_DISPLAY only with clipboard copy

Claude Code copies with `wl-copy` only when `WAYLAND_DISPLAY` is set.
Users set it in `[env]` by hand, also in sandboxes without the clipboard
`copy` grant. There `wl-copy` failed and Claude Code did not use its
other copy methods.

The setup script now keeps the binary in the standard native location
`~/.local/share/claude/versions/<version>`. One wrapper script goes to
both `~/.local/bin/claude` and `/usr/local/bin/claude`. It:

1. picks the newest version file (`ls | grep -x '[0-9][0-9.]*' | sort -V
   | tail -n 1`). The filter skips partial downloads. `sort -V` works
   with busybox (Alpine) and GNU coreutils (Debian).
2. sets `WAYLAND_DISPLAY` (default `airlock-0`) when the copy FIFO
   `/run/airlock/clipboard.copy` exists. airlockd makes it only when the
   host grants copy. Else it unsets the variable.
3. `exec`s the version with `"$@"`.

Updates write new versions to the same directory. Since v2.1.207 they
keep a custom `~/.local/bin/claude`. The wrapper never runs `claude` from
`PATH`, so it cannot run itself. Background updates stay off
(`DISABLE_AUTOUPDATER=1`).

Sandboxes from the old layout (binary at `/usr/local/bin/claude`) keep
working until a new install. A re-run of the script installs again,
because `~/.local/bin/claude` is missing.
