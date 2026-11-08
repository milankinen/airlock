# Deprecate `--sandbox-cwd`

## Goal and user-visible behavior

`airlock start --sandbox-cwd <dir>` is deprecated. The option still
works, but `airlock start` shows a warning when the user gives it. The
`--help` text and the manual do not show the option. It stays only for
backward compatibility. A future release removes it.

## Implementation

* `cmd_start::run` shows the warning after the log setup, before the
  sandbox lookup. Thus each use shows the warning, also when the start
  fails later.
* The text is `messages::start::SANDBOX_CWD_DEPRECATED`. The yellow `!`
  mark is the same as for the deprecated user-level config files.
* The sandbox process, `airlock exec` and `airlock acp` do not change.
  They still use the guest cwd that the last start recorded.
