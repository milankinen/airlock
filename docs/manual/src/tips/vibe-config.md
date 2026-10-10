# Vibe coding configuration

Sometimes you want to "vibe code": point an agent at a random directory
without any extra setup. airlock's configuration is
[hierarchical](../configuration.md#file-hierarchy), so you can put your
personal defaults in `~/.airlock/airlock.toml`. They apply to every
project sandbox:

```toml
# ~/.airlock/airlock.toml
[vm]
cpus = 4
memory = "8 GB"

[network]
policy = "deny-by-default"
```

A user config cannot enable [packs](../packs.md). Instead, the
[setup wizard](../usage/starting-sandbox.md) asks for them on
the first start in each directory.

## Running your setup

Now `cd` into any directory and run:

```bash
airlock start --monitor
```

The first start opens the wizard. Select the agent and the tools, and
press Enter. Later starts use the same config and sandbox. `airlock rm`
removes the sandbox and the local config.
