# GitHub Copilot CLI

The `copilot-cli` preset bundles the sandbox setup for the
[GitHub Copilot CLI](https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli).
It keeps the Copilot OAuth token on the host and scopes network
access to the GitHub endpoints Copilot actually uses.

## What the preset does

The sandbox sees a placeholder token, and airlock swaps in the real
token at the host boundary — only on the specific paths Copilot
uses.

- **Your token stays on the host.** Copilot requests are intercepted
  on `api.github.com` and `*.githubcopilot.com`, and the real
  `Authorization` header is injected there at host side. On
  `api.github.com` the injection is path-scoped to
  `/copilot_internal/*`, so any other GitHub API call an agent might
  make will not receive the Copilot token.
- **Only Copilot endpoints are reachable** (`api.github.com` and
  `*.githubcopilot.com`). Everything else stays blocked by your
  deny-by-default policy.
- **Your Copilot session survives.** `~/.config/gh` is mapped to
  `~/.airlock/copilot/` on the host, so the `gh auth` state and
  Copilot preferences carry over between sandboxes.

## Example `airlock.toml`

```toml
presets = ["copilot-cli"]

[network]
policy = "deny-by-default"

[vm]
image = "docker/sandbox-templates:copilot-docker"
```

The `docker/sandbox-templates:copilot-docker` image ships with `copilot`
already installed. For a real project, you might prefer your own
[project-specific image](../tips/mise.md#building-a-local-image-with-docker).

## Providing the GitHub token

Create a dedicated fine-grained PAT with the Copilot scopes at
<https://github.com/settings/tokens>.

Store your PAT in the airlock
[secret vault](../secrets.md) under the name `COPILOT_GITHUB_TOKEN`:

```bash
airlock secrets add COPILOT_GITHUB_TOKEN
```

## Running it

```bash
airlock start --monitor -- copilot
```
