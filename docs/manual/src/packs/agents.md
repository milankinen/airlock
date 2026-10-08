# Agents

An agent pack installs a coding agent into the sandbox. It also allows
the hosts of the agent and keeps the agent settings on the host, so all
sandboxes with the same agent share them.

- [Claude Code](./agents/claude-code.md) (`claude`)
- [OpenAI Codex](./agents/codex.md) (`codex`)
- [GitHub Copilot CLI](./agents/copilot.md) (`copilot`)

Claude Code and Codex sign in inside the sandbox, but the real tokens
stay on the host. See
[Network services](../configuration/network.md#network-services).
