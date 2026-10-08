# Editor integration with ACP

The [Agent Client Protocol](https://agentclientprotocol.com/) (ACP) lets
an editor use a coding agent in its own UI. The agent packs can install
the ACP tooling of the agent into the sandbox:

| Pack                                      | ACP command        | How to enable     |
|-------------------------------------------|--------------------|-------------------|
| [`claude`](../packs/agents/claude-code.md) | `claude-agent-acp` | `acp = true` arg  |
| [`codex`](../packs/agents/codex.md)       | `codex-acp`        | `acp = true` arg  |
| [`copilot`](../packs/agents/copilot.md)   | `copilot --acp`    | Always available  |

```toml
[packs]
claude = { version = "1", args = { acp = true } }
codex = { version = "1", args = { acp = true } }
```

The editor starts the ACP command in the project directory and talks to
it over stdin and stdout. `airlock exec` does the same, so a normal
`airlock exec -- <acp-command>` connects the editor to the agent in the
sandbox.

## Setup

1. Open a terminal in the project directory and start the sandbox:

   ```bash
   airlock start
   ```

   Keep the sandbox running. `airlock exec` works only when the sandbox
   runs.

2. In the editor, add a custom agent that runs the ACP command with
   `airlock exec`. Most editors with ACP support let you add custom
   agents. For example, JetBrains IDEs read them from
   `~/.jetbrains/acp.json`:

   ```json
   {
     "agent_servers": {
       "airlock-claude": {
         "command": "airlock",
         "args": ["x", "claude-agent-acp"],
         "env": {}
       },
       "airlock-codex": {
         "command": "airlock",
         "args": ["x", "codex-acp"],
         "env": {}
       }
     }
   }
   ```

   If the editor does not find `airlock` on its `PATH`, use the absolute
   path of the binary, for example `/Users/me/.local/bin/airlock`.

3. Select the agent in the editor. The agent runs in the sandbox, with
   the network rules and secrets of the sandbox.
