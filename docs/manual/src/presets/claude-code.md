# Claude Code

The `claude-code` preset bundles the sandbox setup for running
[Claude Code](https://docs.claude.com/en/docs/claude-code/overview)
inside airlock. It wires up the network rules, credential handling,
and settings persistence so you only need to pick an image that
ships the `claude` CLI and drop the preset into your config.

## What the preset does

The real OAuth token stays on the host; the VM only sees a
placeholder. The token is injected into Anthropic API requests at
the host boundary, so it is never exposed to processes running
inside the sandbox.

- **Your token stays on the host.** Requests to `api.anthropic.com`
  are intercepted by airlock on the host and the real
  `Authorization` header is injected there. Inside the VM,
  `CLAUDE_CODE_OAUTH_TOKEN` is a placeholder value.
- **Only Anthropic endpoints are reachable** (`api.anthropic.com`,
  `claude.ai`, `downloads.claude.ai`, `platform.claude.com`).
  Everything else stays blocked by your deny-by-default policy.
- **Claude knows it's sandboxed.** `IS_SANDBOX=1` is set so Claude
  skips host-only behaviour, and `NODE_EXTRA_CA_CERTS` points at the
  airlock CA so the middleware's TLS interception is trusted.
- **Your onboarding survives.** `~/.claude` and `~/.claude.json`
  inside the sandbox are backed by `~/.airlock/claude/settings` and
  `~/.airlock/claude/claude.json` on the host, so login state,
  preferences, and project memory carry over between sandbox runs.
  Disable either mount in `airlock.local.toml` if you prefer a
  fresh sandbox each time.

## Example `airlock.toml`

```toml
presets = ["claude-code"]

[network]
policy = "deny-by-default"

[vm]
image = "docker/sandbox-templates:claude-code"
```

The `docker/sandbox-templates:claude-code` image ships with `claude`
already installed. For a real project, you might prefer your own
[project-specific image](../tips/mise.md#building-a-local-image-with-docker).

## Providing the OAuth token

The middleware expects `CLAUDE_CODE_OAUTH_TOKEN` on the **host**.
Get one by running `claude setup-token` outside the sandbox.

Store the token in the airlock
[secret vault](../secrets.md) under the name `CLAUDE_CODE_OAUTH_TOKEN`:

```bash
airlock secrets add CLAUDE_CODE_OAUTH_TOKEN
```

## Running it

```bash
airlock start --monitor -- claude --dangerously-skip-permissions
```
