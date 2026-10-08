# Packs

A pack is a built-in bundle that ships with airlock. A pack can:

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

airlock has three kinds of packs:

- [Distros](./packs/distros.md) set the base image
- [Agents](./packs/agents.md) install coding agents
- [Tools](./packs/tools.md) install languages and other tools

## Enabling packs

The [setup wizard](./usage/starting-sandbox.md#setup-wizard) writes the
first `[packs]` table for you. You can also write it yourself:

```toml
[packs]
debian = { version = "1" }
claude = { version = "1" }
rust = { version = "1", args = { toolchain = "nightly" } }
```

Each entry has these keys:

| Key       | Description                                                  |
|-----------|--------------------------------------------------------------|
| `version` | Pack version (required). A string or an integer, e.g. `"1"`. |
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

Run `airlock show` to see the result. See the
[built-in packs](https://github.com/milankinen/airlock/tree/main/packs)
for the configuration of each pack.

## Limitations

- User-level config files (`~/.airlock.toml`, `~/.airlock/config.toml`,
  `~/.airlock/airlock.toml`) cannot enable packs. Enable them in the
  project config.
- Agent and tool packs need a base image that runs as `root` and is
  based on Alpine, Debian or Ubuntu.
