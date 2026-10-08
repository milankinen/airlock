# Wizard default start option from user settings

The setup wizard of `airlock start` has a start bar with three options:
`start` (local config in `.airlock/airlock.toml`), `start and share`
(shareable config in `airlock.toml`) and `cancel`. Before this change, the
bar always opened on `start`.

## Change

- New user setting `[wizard_defaults] start` in `~/.airlock/settings.*`.
  Values: `start-and-share` (default) and `start`. It is a personal
  preference, so it is in the user settings, not in the project config.
- The default is now `start-and-share`. Thus Enter on the start bar writes
  a shareable `airlock.toml` unless the user sets `start`.
- `settings::WizardStart` is a separate enum from `form::StartChoice`.
  The settings value cannot be `cancel`, and the settings names
  (kebab-case) stay independent of the view labels. A `From` impl maps it
  to the start bar option.
- `cmd_start` passes `context.settings.wizard_defaults.start` to
  `load_or_generate_config`, then `run_wizard` and `Form::new` take it.
  The answer check (`Input`) does not need it, so it is a separate
  argument.

## Tests

- `settings.rs`: the default is `start-and-share`, and a file with
  `wizard_defaults.start = "start"` overrides it.
- `test_wizard.rs`: with `start and share` first, Enter, Enter gives a
  shareable target. ← then Enter gives a local target.
