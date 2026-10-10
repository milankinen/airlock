# Remove the `wizard_defaults` user setting

The `[wizard_defaults] start` user setting (see
`2026-10-08-wizard-default-start-option.md`) selected the first option of
the start bar of the setup wizard. It is removed. The start bar now always
opens on `start and share`, the old default. The user selects `start`
with the arrow keys to write a local config.

## Change

- `settings`: the `wizard_defaults` field, `WizardDefaults` and
  `WizardStart` are removed. A settings file that still has
  `[wizard_defaults]` is not an error. smart-config ignores unknown keys,
  as it does for other unknown settings.
- `form::Form::new` has no `start` argument. It sets
  `StartChoice::StartAndShare`. The `From<WizardStart>` impl is removed.
- `load_or_generate_config`, `run_wizard` and `ask_config` have no
  `start` argument. `cmd_start` no longer reads the setting.
- Manual: `usage/starting-sandbox.md`, `tips/vibe-config.md` and
  `technical/project-layout.md` no longer show the setting.

## Tests

- `test_wizard.rs`: the tests open the form on `start and share`. The
  user image test presses Left to select `start`. The cancel test needs
  one Right press. The test of the setting is removed, because the first
  test covers the default and the user image test covers Left.
- `settings` tests no longer check the setting.

## Later change

`2026-10-10-wizard-create-airlock-toml-checkbox.md` replaces the
`start and share` option with the `Create airlock.toml` checkbox. That
checkbox now chooses between a local and a shareable config.
