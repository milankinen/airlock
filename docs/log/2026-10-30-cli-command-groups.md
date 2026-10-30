# Command groups in the top-level help

The top-level help listed all commands in one "Commands" section. The
list grew to 12 commands, so the help now shows them in three sections:

* **Sandbox**: commands for the sandbox of the current project
  (`start`, `exec`, `info`, `config`, `network`, `remove`)
* **Management**: commands for all sandboxes, templates and secrets
  (`sandboxes`, `templates`, `secrets`)
* **Miscellaneous**: `acp`, `self-update` and `help`

Clap has no support for more than one subcommand heading. Thus
`cli::help_template` writes the sections itself in place of
`{all-args}`, and adds an "Options" title above the clap `{options}`
tag. The names and the about texts come from the clap command, so they
stay the same as the clap definitions. Only the group of each command is
a fixed table (`COMMAND_GROUPS`). A visible command that is not in the
table shows in the last section, so a new command never disappears from
the help.

`main` builds the root command before it makes the template. The build
adds the `help` command, and the template must list it. The section
titles use bold and underline and the command names use bold, the same
as the clap default styles. `console` drops the styles when color is off.

Each section title has an empty line below it, and the "Options" title
too. The help also ends with an empty line. Clap removes the trailing
line breaks from the template, so `main` gets the help as a clap error,
prints it and then writes the empty line to the same stream. Bare
`airlock` writes the help to stderr, `airlock --help` to stdout. This
also adds the empty line to the help of the subcommands.
