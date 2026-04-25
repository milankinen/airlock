# Configuration

airlock is configured through TOML files. The main configuration file is
`airlock.toml` at the project root, and it's meant to be committed to version
control so that every team member gets the same sandbox setup.

## File hierarchy

Configuration is loaded from up to four locations, with later files overriding
earlier ones:

1. `~/.airlock/config.toml` or `~/.airlock.toml` — user-level settings (e.g. preferred CPU/memory)
2. `airlock.toml` — project config (checked into version control)
3. `airlock.local.toml` — local overrides (gitignored)

This layering means a company can ship global defaults, individual developers
can set personal preferences, and each project defines its own sandbox — with
room for local tweaks that don't affect the team.

JSON and YAML files are also accepted (e.g. `airlock.json`, `airlock.yaml`).
For each slot, the first matching extension in the order `.toml`, `.json`,
`.yaml`, `.yml` wins.

## Minimal example

A project that uses Ubuntu with a Rust toolchain preset:

```toml
presets = ["rust"]

[vm]
image = "ubuntu:24.04"
cpus = 4
memory = "4 GB"
```

This is enough to get a working sandbox. The `rust` preset adds network rules
for `crates.io` and related hosts, so `cargo build` works out of the box.

## Sandbox state

Sandbox runtime state (disk image, CA certificate, overlays, logs) is stored
in `.airlock/` inside the project directory. This directory is automatically
excluded from version control. Running `airlock rm` removes it entirely;
`airlock start` recreates it from scratch.

## Merging behaviour

When multiple configuration files are present, they're merged with these
rules:

- Object fields are merged recursively (e.g. `[vm]` settings from different
  files are combined, not replaced)
- Arrays are concatenated (e.g. preset lists from different levels stack)
- Scalar values are overridden by later files
- A `null` value never overwrites an existing value

This means you can set `vm.cpus = 2` in your user config and only override
`vm.image` in the project config — both settings apply.

## Overriding with `enabled`

Every named configuration entry — network rules, mounts, disk caches, and
socket forwards — has an `enabled` flag that defaults to `true`. Combined with
the hierarchical config loading, this gives individuals full control over
shared configurations.

For example, if the project `airlock.toml` defines a mount and a network rule
via a preset, a developer can disable either one in their `airlock.local.toml`
without modifying the shared config:

```toml
# airlock.local.toml — personal overrides, not committed

[mounts.ssh-config]
enabled = false

[network.rules.alpine-packages]
enabled = false
```

This works at every level. A company-wide global config can define baseline
rules, a project config can add its own, and any developer can selectively
disable what doesn't apply to them — all without editing files that belong to
someone else.
