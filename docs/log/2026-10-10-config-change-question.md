# Ask before a start with a changed config

## Problem

A config change applied to the next start without a question. Code in
the sandbox can edit `airlock.toml` in the project mount, for example to
add mounts, open the network policy or add env variables. The user
starts the sandbox again and gets the changed config without notice.

## Change

- New step `start::config_change`. The sandbox step keeps the config
  files of the run (the user, local and project layers, as
  `LayeredConfig` JSON) in `run.json`, field `last_run_config`.
- On a start, the check resolves the stored layers again and compares the
  final values with the final values of the run. It runs after the
  sandbox lock and the move question, and before the install state read,
  the tool questions and the image step. A change gives the question
  "Sandbox configuration has changed since last run:" with "Abort"
  (first, default), "Accept changes" and "Continue with old
  configuration". An
  unindented gray hint tells to press `s`. `s` shows and hides a colored
  unified diff of the final values (as TOML) between the hint and the
  options.
- "Continue with old configuration" gives the resolved config of the last
  run to the rest of the start: `ensure_sandbox` takes the resolved config
  by value and returns the config of the run in `EnsuredSandbox`. The
  `[env]` check runs again for the old config. The old config is not
  saved, so the next start asks again.
- If the stored layers do not resolve any more (for example a removed
  preset name), the question has a note and no "continue" option.
- `--yes` selects the default, abort: the start stops with exit code 2.
  The same applies without a terminal. A script must not accept a change
  that code in the sandbox can make. The new flag
  `--allow-configuration-changes` accepts the changes without a question,
  also together with `--yes`.
- No copy (a new sandbox, or a sandbox of an older airlock): no question.
  The start saves a copy.
- The copy is saved at the end of the sandbox step. If a later question
  ends the run, the next start asks again.
- The `--network` policy is not part of the comparison, because it applies
  to one run only. `ResolvedConfig` has the new field `configured_policy`,
  which keeps the policy of the config. The override also applies to the
  old config (`SandboxOptions.overrides`).
- The comparison and the diff replace masked `[env]` values with a short
  sha256. Thus the diff on the screen shows no secret, and a changed value
  still shows as a change.
- Prompts: `Choose::ask_with_details` takes `Details` (key, show and
  hide hints, lines). The lines are part of the prompt view, so the key
  can hide them again. Text printed above the view stays on the terminal
  and cannot be hidden.
- New dependency `similar` for the diff.

## Alternatives

- Store only the final values. They cannot give the old config back: the
  pack list is not in them, and masked values are hashed. The pack list
  is necessary, because the install step uses it. The layers resolve
  through the normal path and give a full config.
- Compare the stored final values with the new ones. Then an airlock
  update that changes pack values asks a question with no config change
  of the user. With the layers, both configs resolve with the same
  airlock.

## Limits

- The stored layers have the config values as written, also literal
  values of masked `[env]` entries. `run.json` is in the sandbox
  directory, next to the CA key in `ca.json`.
- A project-owned sandbox (`<project>/.airlock/sandbox/`) is visible to
  the guest, so the guest can also edit the copy. The check protects
  managed sandboxes in the data directory.
- A `run.json` that is not valid reads as empty, as before. Then there is
  no copy and no question. Only a process that can write the sandbox
  directory can cause this, and such a process can also write the copy.
- Host env values (`${VAR}`) are not resolved in the copy. A changed host
  variable is not a config change.

## Tests

- `start/tests/test_config_change.rs`: a changed config without a terminal
  or with `--yes` stops the start before the image step,
  `--allow-configuration-changes` accepts it, and a
  sandbox without a copy starts and gets one.
- `start/config_change.rs`: "continue with old configuration" gives the
  config of the last run, a stored config that does not resolve gives a
  note, the `--network` override and masked env values, and the TOML diff
  of two configs.
- `StartProject::accept_config` lets the tool and image change tests skip
  the config question.
- CLI bats: a changed config without a terminal or with `--yes` is exit
  code 2, and `--allow-configuration-changes` continues. VM bats
  `check_removed_pack` now expects the config message. Not run here (no
  KVM).
