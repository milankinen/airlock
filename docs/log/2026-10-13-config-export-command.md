# `airlock config` export command

Users had no way to get the config of a project as one file.

## Change

- New command `airlock config [--resolved] [--format=<toml|json|yaml>]`. It
  prints to stdout. The default format is TOML.
- `airlock sandboxes config <id>` gives the same output for any
  registered sandbox.
- Default output: the project config. It is the merge of `airlock.toml`,
  `airlock.local.toml` and the local project config. The user files
  (`~/.airlock*`) are not in it. `[packs]` and `presets` stay as written.
- `--resolved`: the final config of the last run. `airlock start` stores the
  layered config, with the user files, in the sandbox. The command
  resolves that copy and merges in the packs, thus the output has no
  `[packs]` table. Changes to the files after the last run are not in it.
  The `--network` override is not in it. The output has the absolute pack
  directory paths and the defaults of this machine (for example the
  memory size), thus it is for this machine only.
- Errors go to stderr as `Cannot export config: ...` with exit code 1.
  `--resolved` without a stored last run is an error. No project config
  is not an error: a warning goes to stderr, and an empty document goes
  to stdout.
- Manual: new "Export the config" section in `usage/managing-sandbox.md`.
- Tests: `tests/cli/config.bats` and
  `config/tests/test_config_export.rs`.

## Alternatives

- Store the resolved config at start and print it with `--resolved`. Not
  done: the config change check stores the layers on purpose (see
  `2026-10-10-config-change-question.md`). The export resolves them with
  the current packs. Pack setup is internal behavior of airlock.
- Keep `[packs]` and `presets` in the `--resolved` output, with flags to
  resolve each part. Not done: a flat document moves the values of lower
  files above the packs, thus it does not give the same config when
  airlock loads it again.
