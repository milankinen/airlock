# Tips and tricks

This section collects practical patterns that occur often in day-to-day
work with airlock. None of this is required reading, but it can save
you some time.

[Pairing with mise](./tips/mise.md) shows how to use mise as a task runner
alongside airlock — installing airlock as a mise tool, building local Docker
images for sandboxes, and loading secrets per task.

[Open-network bootstrap](./tips/init-with-open-network.md)
shows how to run a one-off init script with `airlock start --network` while
keeping `deny-by-default` in `airlock.toml`.

[Editor integration with ACP](./tips/editor-acp.md) shows how to connect
an editor to a coding agent in the sandbox.

[Experimental: Claude hooks](./tips/experimental-claude-hooks.md) shows how
to tell Claude Code when the network policy denies a connection.
