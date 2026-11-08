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

`airlock acp <command>` starts a proxy that lets the editor talk to the
agent over the ACP protocol. When the editor starts or loads a session,
or lists the sessions of a directory, the proxy connects to the sandbox
of that directory. If that sandbox is running, the proxy attaches to it.
Otherwise, it starts the sandbox.

The sandbox must first be created with `airlock start`. ACP sessions use
the config of that sandbox, and a sandbox started by `airlock acp` never
picks up config changes.

## Setup

1. Start the sandbox once in the project directory, so that it has a
   config:

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

## Many projects

An editor can use one `airlock acp` process for many projects.

* Each session starts its own agent process in the sandbox of its
  directory. Two sessions in one sandbox use two agent processes.
* Each session is separate. A session gets only its own messages. The
  agent request IDs are unique for the editor.
* When a session ends, its agent stops.
* An agent process keeps its sandbox running. When the last agent in a
  sandbox stops, the sandbox stops if no other airlock command uses it.
* A `session/close` response ends the session. A load or resume of an
  open session stops the old session first.
* When an agent exits, the open requests of its session get an error.
  The session gets a notice, or an agent message if the editor shows no
  notices.
* The next request in that session starts a new agent and opens the
  session again.
* The proxy runs until the editor closes it.
* A request that needs a session but has none gets "not supported".
* A signal stops all agents and the proxy waits for them. A second signal
  kills them. The exit code is the first exit code of an agent that is
  not 0.

## Session list and delete

* A `session/list` with a directory runs in the sandbox of that
  directory. airlock starts a short agent process for the list and stops
  it after the list. If the sandbox does not run, airlock starts it for
  the list too. The session paths in the rows are host paths.
* A `session/list` without a directory gives an empty list.
* A request for a session that a list named, for example `session/delete`,
  runs the same way in the directory of that list.
* `authenticate`, `logout` and `providers/*` get "not supported".

## Restrictions

By default, these editor features work:

* permission prompts and form prompts
* session updates, sub-agents, plans and the session list
* terminal output (the editor only shows it)
* the read-only features of JetBrains AIR

The proxy blocks these editor features, because they act on the host:

* host commands (`terminal`) and host files (`fs`)
* editor login (`auth`, `terminal-auth`) and URL prompts
  (`elicitation.url`)
* plan files (`planFile`) and file change reports
  (`agentFileChangeReport`)
* MCP servers in the editor
* unknown features and methods

The proxy lets through only the names of JetBrains AIR features that are
safe. It refuses the other agent requests. It drops the other agent
notifications.

`--pass-client-capabilities` gives the agent all editor features.

`airlock acp` cannot start a stopped sandbox when the vault is an
encrypted file and `AIRLOCK_VAULT_PASSPHRASE` is not set in the editor's
environment.

> [!WARNING]
> With `--pass-client-capabilities`, the agent can run commands and
> change files on the host. Use it only for an agent that you trust.

## Agent features

The proxy answers the editor handshake before the agent starts. It tells
the editor the features of the agent from a preset. The proxy selects the
preset from the command name:

| Command            | Preset                                        |
|--------------------|-----------------------------------------------|
| `claude-agent-acp` | Claude                                        |
| `codex-acp`        | Codex                                         |
| `copilot --acp`    | Copilot                                       |
| Other              | `loadSession`, `promptCapabilities.image`, `promptCapabilities.embeddedContext`, `steering` |

A launcher such as `npx` gets the default preset. For a launcher, use
`--set-agent-capabilities`.

`--set-agent-capabilities=<list>` replaces the preset. The list has
comma-separated names. An empty list gives no features. An unknown name
is an error, and the message lists the valid names.

| Name                                                     | Feature                      |
|----------------------------------------------------------|------------------------------|
| `loadSession`                                            | Session load                 |
| `promptCapabilities.image`, `.audio`, `.embeddedContext` | Prompt content types         |
| `sessionCapabilities.list`, `.resume`, `.close`, `.delete`, `.fork` | Session actions |
| `sessionCapabilities.additionalDirectories`, `.systemPrompt`, `.subagents` | Session options |
| `_meta.authStatus`                                       | Login status of the agent    |
| `steering`                                               | Send a message to a running turn |
| `air:goal`                                               | AIR goals                    |
| `air:sessionFailure`, `air:nativeSubagentSessions`, `air:asyncTasks`, `air:customInstructions`, `air:recommendedValue`, `air:diffPatch`, `air:rawInputRendering`, `air:planContentDelta`, `air:backgroundSubagents`, `air:sessionIndex` | JetBrains AIR features |

The editor gets the `air:` names only if it declares AIR support.

Example for a custom agent that starts through `npx`:

```json
"args": ["acp", "--set-agent-capabilities=loadSession,promptCapabilities.image,sessionCapabilities.list,steering", "npx", "my-acp-agent"]
```

When the real agent starts, the proxy refuses editor requests for
features that it does not have.

## Editor MCP servers

`--proxy-mcp-servers` gives the agent the MCP servers of the editor.
airlock serves them from the host and the agent connects to them through
`http://acp.mcp.airlock/<key>/<name>`. They stop when the session ends.

> [!WARNING]
> With `--proxy-mcp-servers`, the MCP tools act on the host with your
> rights. Use it only for editor servers that you trust.

## Options

| Option                       | Effect                                                    |
|------------------------------|-----------------------------------------------------------|
| `--pass-client-capabilities` | Give the agent all editor features                        |
| `--set-agent-capabilities <list>` | Replace the [agent feature](#agent-features) preset. Comma-separated names, empty for none |
| `--proxy-mcp-servers`        | Bridge the MCP servers of the editor to the agent         |
| `--follow-git-worktree`      | In a [git worktree](../usage/running-commands.md#git-worktrees), also use the sandbox of the main repository |
| `-e KEY=VALUE`               | Set an environment variable for the agent, as in `airlock exec` |
| `-l`, `--login`              | Start the agent in a login shell, as in `airlock exec`    |
| `--log-level <level>`        | Set the level of the log file. The default is `info`      |

## Agent login

The login buttons of the editor do not work. The proxy refuses
`authenticate` and `logout`. Log in to the agent in the sandbox, for
example with `airlock exec`. See the
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

* `info` (default): shows the editor features as sent and after the
  filter, sessions, sandboxes, agent start and exit. The lines name the
  sandbox of each agent. Use it to find why an agent does not start or
  which agent exited.
* `debug`: also shows the methods of dropped and refused agent messages,
  and refused editor requests. Use it when an editor feature does not
  work.
* `trace`: also shows each full message between airlock and an agent as
  `<from>: <message>` with the sandbox name. `agent` is a message from
  the agent. `airlock` is a message from airlock to the agent. Airlock
  hides the values of MCP server `env` and headers. The log has no
  messages between the editor and airlock.

airlock cuts the file to its last 1 MiB when it grows.

> [!WARNING]
> The `trace` level logs prompts and file content. After debugging, set
> the level back and delete the log.
