# Allow symlinks to config files

The host IO hardening refused a project `airlock.toml` or
`airlock.local.toml` that is a symlink to a file outside the project.
Users who keep these files in a dotfiles repo could not start airlock.

## Design

* User files (`~/.airlock/airlock.<ext>`, `~/.airlock/config.<ext>`,
  `~/.airlock.<ext>`) and project files (`<project>/airlock.<ext>`,
  `<project>/airlock.local.<ext>`) follow symlinks to anywhere.
* The local project file (`<local_dir>/airlock.<ext>`) must not be a
  symlink. Airlock manages it and never makes it a link.
* The removed "link stays in the project" mode and its error text are
  gone. No other caller used them.

## Reason

The guest can write the project dir and can replace a project config
file with a link to a host file. The config change check of
`airlock start` compares the final config with the config of the last
run. A link that changes the config thus gives the question before the
sandbox starts. That check covers this case, so the scope limit is not
necessary, and it broke a normal setup.
