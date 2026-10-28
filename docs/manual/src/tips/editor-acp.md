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

`airlock acp <command>` connects the editor to the ACP command in the
sandbox. The editor can start it in any directory. The agent starts at
the first session, in the running sandbox of the session directory.

## Setup

1. Start the sandbox in the project directory and keep it running:

   ```bash
   airlock start
   ```

2. In the editor, add a custom agent that runs `airlock acp`. For
   example, JetBrains IDEs read custom agents from
   `~/.jetbrains/acp.json`:

   ```json
   {
     "agent_servers": {
       "airlock-claude": {
         "command": "airlock",
         "args": ["acp", "claude-agent-acp"]
       }
     }
   }
   ```

   If the editor does not find `airlock`, use the absolute path of the
   binary.

3. Select the agent in the editor.

One `airlock acp` process serves one sandbox. It refuses sessions in
other sandboxes.

## Restrictions

By default, the agent cannot use the editor features that act on the
host: host commands, host files, terminal login, URL prompts, MCP
servers in the editor and unknown methods. Permission prompts, form
prompts and session updates work.

`--pass-client-capabilities` gives the agent all editor features.

> [!WARNING]
> With `--pass-client-capabilities`, the agent can run commands and
> change files on the host. Use it only for an agent that you trust.

## Options

| Option                       | Effect                                                    |
|------------------------------|-----------------------------------------------------------|
| `--pass-client-capabilities` | Give the agent all editor features                        |
| `--follow-git-worktree`      | In a [git worktree](../usage/attaching-to-running-sandbox.md#git-worktrees), also use the sandbox of the main repository |
| `-e KEY=VALUE`               | Set an environment variable for the agent, as in `airlock exec` |
| `-l`, `--login`              | Start the agent in a login shell, as in `airlock exec`    |
| `--log-level <level>`        | Set the level of the log file. The default is `info`      |

## Agent login

The login buttons of the editor do not work. Log in to the agent in the
sandbox, for example with `airlock exec`. See the
[agent pack](../packs/agents/claude-code.md) documentation.

## ACP debugging

If the agent does not start or acts in an unexpected way, read `acp.log`
in the
[airlock data directory](../usage/managing-sandbox.md#where-airlock-keeps-sandbox-data).
Each line starts with the process ID of `airlock acp`.

To get more details, add `--log-level` to the agent arguments in the
editor and start the agent again:

```json
"args": ["acp", "--log-level", "debug", "claude-agent-acp"]
```

* `info` (default): shows sessions, sandboxes, agent start and exit.
  Use it to find why the agent does not start.
* `debug`: also shows other methods and refused messages. Use it when
  an editor feature does not work.
* `trace`: also shows each full message as `<from> -> <to>: <message>`,
  for example `client -> agent`. `airlock` is the proxy. Use it to see
  the full ACP traffic.

airlock cuts the file to its last 1 MiB when it grows.

> [!WARNING]
> The `trace` level logs prompts and file content. After debugging, set
> the level back and remove the log.
