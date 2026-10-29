# Packs

A pack is a bundle of sandbox setup. airlock ships with built-in packs.
You can also write your own packs (see [Pack sources](#pack-sources)). A
pack can:

- Set the base image of the sandbox
- Install software into the sandbox, for example a coding agent or a
  language toolchain
- Add the configuration that the software needs: network rules,
  environment variables, mounts and daemons

Without packs, you must build an image with your tools and find out
which hosts each tool needs. With packs, you select the tools and airlock
does the rest.

airlock installs the packs when it creates the sandbox, before the
session starts. The software stays on the sandbox disk. To get a newer
version of a tool, re-create the sandbox with `airlock rm`.

Each pack has a kind. The setup wizard shows the packs in one section for
each kind:

- [Distros](./packs/distros.md) set the base image
- [Agents](./packs/agents.md) install coding agents
- [Tools](./packs/tools.md) install languages and other tools
- Other packs do not have one of these kinds, for example a pack that
  adds only config

## Pack sources

airlock finds packs in three sources:

| Source        | `[packs]` key | Where airlock finds the pack           |
|---------------|---------------|----------------------------------------|
| Built-in pack | `name`        | In the airlock binary                  |
| Project pack  | `"./path"`    | In the project, relative to the root   |
| User pack     | `"~name"`     | In a user pack directory               |

Built-in packs ship with airlock. Each built-in pack entry must set
`version`.

A project pack is a directory in the project, for example `./setup`. The
path starts with `./` and cannot go out of the project. Commit the pack
with the project. Then each person on the team gets the same setup.

A user pack is a directory in your personal pack directory. It is for
your own tools and preferences. See the `packs.dirs`
[setting](./configuration.md#user-settings) for the default directory
and how to change it. The setup wizard shows user packs with the mark
`(local)`.

User packs and project packs have no versions. See
[Authoring packs](./packs/authoring.md) to write your own packs.

> [!IMPORTANT]
> Do not put user packs in a shared `airlock.toml`. Other persons do not
> have your user packs, and their start fails. Put user packs in
> `airlock.local.toml`, or select them in the wizard for a personal
> sandbox config.

## Enabling packs

The [setup wizard](./usage/starting-sandbox.md) writes the
first `[packs]` table for you. You can also write it yourself:

```toml
[packs]
debian = { version = "1" }
claude = { version = "1" }
rust = { version = "1", args = { toolchain = "nightly" } }
"~my-tools" = {}
"./packs/setup" = { args = { verbose = true } }
```

Each entry has these keys:

| Key       | Description                                                  |
|-----------|--------------------------------------------------------------|
| `version` | Pack version. A string or an integer, e.g. `"1"`. Required for built-in packs. Not allowed for user packs and project packs. |
| `args`    | Pack options. Each pack page lists its args and defaults.    |
| `enabled` | Set `false` to turn off a pack. Default `true`.              |

The entries merge across the config files. For example, to turn off a
pack for yourself only, write this in `airlock.local.toml`:

```toml
[packs]
docker = { enabled = false }
```

## Overriding pack configuration

Under the hood, packs set normal airlock configuration. You can override
each value in your own config:

```toml
[packs]
nodejs = { version = "1" }

# Do not allow the npm and Yarn registries of the pack
[network.rules.nodejs-packages]
enabled = false
```

Run `airlock info` to see the result. See the
[built-in packs](https://github.com/milankinen/airlock/tree/main/packs)
for the configuration of each pack.

## Limitations

- User-level config files (`~/.airlock.toml`, `~/.airlock/config.toml`,
  `~/.airlock/airlock.toml`) cannot enable packs. These files are
  deprecated. Move their config to a user pack. Then add the pack to
  the `packs.defaults` [setting](./configuration.md#user-settings), and
  the setup wizard selects it for each new sandbox.
- The setup wizard does not show project packs.
- Templates cannot have project packs.
- Agent and tool packs need a base image that runs as `root` and is
  based on Alpine, Debian or Ubuntu.
