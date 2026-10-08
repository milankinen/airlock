# Configuration

You configure airlock through TOML files. The main configuration file is
`airlock.toml` at the project root. Commit it to version control so that
every team member gets the same sandbox setup.

## File hierarchy

airlock loads configuration from up to six locations. Later files override
earlier ones:

1. `~/.airlock/airlock.toml` — user-level settings (e.g. preferred CPU/memory)
2. `~/.airlock/config.toml` — alternative user-level settings file
3. `~/.airlock.toml` — alternative user-level settings file
4. `.airlock/airlock.toml` — local project config (not in version control)
5. `airlock.toml` — project config (checked into version control)
6. `airlock.local.toml` — local overrides (gitignored)

This layering lets a company ship global defaults and each developer set
personal preferences. Each project defines its own sandbox, with room for
local tweaks that don't affect the team.

The [setup wizard](./usage/starting-sandbox.md#setup-wizard) writes the
first project config to `.airlock/airlock.toml` (`start`) or to
`airlock.toml` (`start and share`). `airlock rm` removes
`.airlock/airlock.toml` together with the sandbox.

airlock also accepts JSON and YAML files (e.g. `airlock.json`, `airlock.yaml`).
For each slot, the first matching extension in the order `.toml`, `.json`,
`.yaml`, `.yml` wins.

## Minimal example

A project that uses Debian with a Rust toolchain:

```toml
[packs]
debian = { version = "1" }
rust = { version = "1" }

[vm]
cpus = 4
memory = "4 GB"
```

This is enough to get a working sandbox. The `rust` [pack](./packs.md)
installs Rust and adds network rules for `crates.io` and related hosts, so
`cargo build` works with no extra rules.

## Sandbox state

airlock stores sandbox runtime state (disk image, CA certificate, logs)
in `.airlock/` inside the project directory. airlock automatically
excludes this directory from version control. `airlock rm` removes it
entirely. `airlock start` recreates it from scratch.

## Merging behaviour

When multiple configuration files are present, airlock merges them with
these rules:

- Object fields merge recursively (e.g. `[vm]` settings from different
  files combine, they do not replace each other)
- Arrays concatenate (e.g. `allow` lists of the same rule from different levels stack)
- Later files override scalar values
- A `null` value never overwrites an existing value

This means you can set `vm.cpus = 2` in your user config and only override
`vm.image` in the project config — both settings apply.

## Overriding with `enabled`

Every named configuration entry — network rules, mounts, disk caches, and
socket forwards — has an `enabled` flag that defaults to `true`. Combined with
the hierarchical config loading, this gives individuals full control over
shared configurations.

For example, a pack in the project `airlock.toml` can define a mount and
a network rule. A developer can disable either one in their
`airlock.local.toml` without modifying the shared config:

```toml
# airlock.local.toml — personal overrides, not committed

[mounts.ssh-config]
enabled = false

[network.rules.debian-packages]
enabled = false
```

This works at every level. A company-wide global config can define baseline
rules, and a project config can add its own. Any developer can selectively
disable what doesn't apply to them, without editing files that belong to
someone else.
