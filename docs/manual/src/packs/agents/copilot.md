# GitHub Copilot CLI

The `copilot` pack installs the
[GitHub Copilot CLI](https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli)
into the sandbox. The ACP support is built in (`copilot --acp`). See [editor integration](../../tips/editor-acp.md).

```toml
[packs]
copilot = { version = "2" }
```

```bash
airlock start --monitor -- copilot
```

The pack has no args.

## Airlock tools

The `copilot@2` pack connects Copilot to the `airlock` MCP server at
`http://admin.airlock/mcp`. Copilot can then use the tools for
[on-demand rules and temporary network access](../../configuration/network.md#on-demand-rules).
The pack also gives the agent the tool instructions at the start of each
session. Hooks tell the agent when the network policy blocked a connection
during a tool call.

Version 1 of the pack has no airlock tools.

The tool names start with `airlock-`, for example `airlock-execute`.

The pack replaces `/usr/local/bin/copilot` with a wrapper. The wrapper adds
the options `--additional-mcp-config` and `--allow-tool=airlock`. The hooks
are in `/etc/github-copilot/policy.d/50-airlock.json`.

## Authentication

### GitHub token

Create a [fine-grained personal access token][new-pat] with the
**Account > Copilot Requests** permission. Store it in the airlock
[secret vault](../../secrets.md):

[new-pat]: https://github.com/settings/personal-access-tokens/new

```bash
airlock secrets add COPILOT_GITHUB_TOKEN
```

airlock reads the token from the host environment first and then from the
vault. The token is optional. If neither has the token, the sandbox starts
without `COPILOT_GITHUB_TOKEN`.

If the token is set, the sandbox sees only a
[masked](../../configuration/env.md#masking) surrogate, so you do not need
`/login`. airlock puts the real token into the requests. For the supported
token types, see the [Copilot CLI authentication docs][copilot-auth].

[copilot-auth]: https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli#authenticating-with-environment-variables
