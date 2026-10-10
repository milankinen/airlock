# Wizard: `Create airlock.toml` checkbox replaces `start and share`

The start bar of the setup wizard had three options: `start` (local
config), `start and share` (shareable `airlock.toml`) and `cancel`. Now a
checkbox chooses where the config goes. This also replaces what the removed `[wizard_defaults]`
setting controlled (see `2026-10-10-remove-wizard-defaults-setting.md`).

## Change

- The start bar has two options: `start` and `cancel`. It opens on
  `start`.
- A new last section "Config" has one checkbox, `Create airlock.toml`
  (`Row::ShareConfig`). It is off by default.
  - Off: `start` writes the local config (`Target::Local`), the same as
    the old plain `start`: in the sandbox directory, or in `.airlock/`
    for a project sandbox. No `airlock.toml`.
  - On: `start` writes `airlock.toml` (`Target::Project`), the same as
    the old `start and share`.
- `form`: `StartChoice::StartAndShare` and `StartChoice::target` are
  removed. `Form` has a `share` flag. Space toggles it on its row. Enter
  on the start bar maps `start` plus the flag to the target.
- `view`: the "Config" section shows the checkbox after "Capabilities".
  The focused row has a short description.
- Manual: `usage/starting-sandbox.md` no longer has a separate setup
  wizard section or wizard details. It says that the wizard selects packs
  and that `Create airlock.toml` gives a config for version control.
  "Configuration basics" names `airlock.toml` and `airlock.local.toml`
  only. `configuration.md` no longer lists the local project config,
  because it is an implementation detail. Links to the removed
  `#setup-wizard` anchor now go to the page.

## Tests

- `test_wizard.rs`: the shared-config test enables the checkbox and then
  starts. The user image test starts at once and gets a local config,
  which covers the default. The cancel test needs one → press.
