# Claude Code

The `claude` pack installs
[Claude Code](https://code.claude.com/docs/en/overview) into the sandbox.

```toml
[packs]
claude = { version = "1" }
```

```bash
airlock start --monitor -- claude --dangerously-skip-permissions
```

| Arg   | Default | Description                                               |
|-------|---------|-----------------------------------------------------------|
| `acp` | `false` | Install the ACP adapter as the `claude-agent-acp` command. See [editor integration](../../tips/editor-acp.md). |

## Authentication

### Login in the application

Start `claude` in the sandbox and sign in with `/login`. The sign-in page
opens in the browser of the host.

airlock keeps the real tokens on the host, encrypted with a key from the
[secret vault](../../secrets.md). Claude Code gets only surrogate tokens.

The sign-in is shared:

- A sign-in in one sandbox applies to all sandboxes with the `claude`
  pack.
- A logout in one sandbox signs out all sandboxes.

### Long-lived Claude token

Create a token on the host with `claude setup-token`, and store it in the
vault:

```bash
airlock secrets add CLAUDE_CODE_OAUTH_TOKEN
```

Then inject the token:

```toml
[env]
CLAUDE_CODE_OAUTH_TOKEN = { value = "${CLAUDE_CODE_OAUTH_TOKEN}", mask = true }

[network.rules.claude-token]
allow = ["api.anthropic.com:443"]
inject = ["CLAUDE_CODE_OAUTH_TOKEN"]
```

The sandbox sees only a [masked](../../configuration/env.md#masking)
surrogate. airlock puts the real token into the requests.

### Anthropic API key

Store the API key in the vault:

```bash
airlock secrets add ANTHROPIC_API_KEY
```

Then inject the key:

```toml
[env]
ANTHROPIC_API_KEY = { value = "${ANTHROPIC_API_KEY}", mask = true }

[network.rules.anthropic-api-key]
allow = ["api.anthropic.com:443"]
inject = ["ANTHROPIC_API_KEY"]
```

The sandbox sees only a masked surrogate. airlock puts the real key into
the requests.
