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

### Token or API key

Set `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) or
`ANTHROPIC_API_KEY` on the host, or store it in the
[secret vault](../../secrets.md):

```bash
airlock secrets add CLAUDE_CODE_OAUTH_TOKEN
```

Claude Code then uses it automatically. The real value stays on the host.
Claude Code gets only a surrogate.
