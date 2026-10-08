# Presets (deprecated)

> **Deprecated:** use [packs](../packs.md) instead. airlock still reads the
> `presets` list, so existing configs work without changes. See
> [Migrating to packs](#migrating-to-packs).

Presets are built-in configuration bundles that ship with airlock. A preset
adds only configuration: network rules, environment variables and mounts.
It does not install software, so the image must contain the tools.

## Using presets

Add presets to the top-level `presets` array in your config:

```toml
presets = ["debian", "rust", "claude-code"]

[vm]
image = "ubuntu:24.04"
```

airlock applies presets as a base layer. Your own configuration always takes
priority and overrides anything a preset defines. You can combine multiple
presets freely.

## Distribution presets

These open network access to the package repositories for each Linux
distribution so that commands like `apt install` and `apk add` work.

- **`alpine`** — Alpine Linux package mirrors
- **`debian`** — Debian and Ubuntu package repositories (including PPAs and
  security updates)
- **`fedora`** — Fedora, CentOS, and RHEL package mirrors
- **`arch`** — Arch Linux and AUR repositories
- **`suse`** — openSUSE and SUSE update servers

Pick the one that matches your base image. For `ubuntu:24.04`, the
`debian` preset is the right choice.

## Language and tool presets

These open network access to language-specific package registries so your
package manager can fetch dependencies.

- **`rust`** — crates.io and Rust toolchain downloads
- **`python`** — PyPI
- **`nodejs`** — npm and Yarn registries
- **`docker`** — starts `dockerd` and allows image pulls from Docker Hub
  and GHCR. The image must contain the Docker engine.

## AI agent presets

These presets configure network rules, credential forwarding and settings
mounts for AI coding agents. The image must contain the agent, for
example a `docker/sandbox-templates` image. Each preset expects a token on
the host, in the environment or in the [secret vault](../secrets.md). The
sandbox sees only a [masked](./env.md#masking) surrogate.

| Preset         | Token on the host         | Allowed hosts | Settings mount |
|----------------|---------------------------|---------------|----------------|
| `claude-code`  | `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) | `api.anthropic.com`, `claude.ai`, `downloads.claude.ai`, `platform.claude.com` | `~/.airlock/claude/settings` → `~/.claude`, `~/.airlock/claude/claude.json` → `~/.claude.json` |
| `openai-codex` | `OPENAI_API_KEY`          | `api.openai.com`, `auth.openai.com` | `~/.airlock/codex` → `~/.codex` |
| `copilot-cli`  | `COPILOT_GITHUB_TOKEN`    | `github.com`, `api.github.com`, `*.githubcopilot.com` | `~/.airlock/copilot-cli` → `~/.copilot` |

Store the token once:

```bash
airlock secrets add CLAUDE_CODE_OAUTH_TOKEN
```

To share your host Claude settings with the sandbox, point the mounts at
them:

```toml
[mounts.claude-settings]
source = "~/.claude"

[mounts.claude-json]
source = "~/.claude.json"
```

## Combining presets with custom rules

A typical project config combines a distribution preset with a language
preset and an agent preset, then adds project-specific rules on top:

```toml
presets = ["debian", "python", "claude-code"]

[vm]
image = "ubuntu:24.04"

[network]
policy = "deny-by-default"

[network.rules.internal-api]
allow = ["api.internal.company.com:443"]

[network.middleware.internal-api-auth]
target = ["api.internal.company.com:443"]
env.TOKEN = "${INTERNAL_API_TOKEN}"
script = '''
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
'''
```

This gives you Debian package repositories, PyPI, Claude API access, and your
internal API — all in a deny-by-default sandbox.

## Overriding preset rules

Since presets are regular configuration applied at a lower priority, you
can override or disable any rule they define. If a preset opens network
access to something you don't need, disable it in your project config or
local overrides:

```toml
# airlock.local.toml
[network.rules.alpine-packages]
enabled = false
```

## Migrating to packs

First read the [packs](../packs.md) documentation. Then replace the
`presets` list with a `[packs]` table. The packs install the software, so
you can remove the agent and tool installs from your image.

| Preset         | `[packs]` entry |
|----------------|-----------------|
| `alpine`       | `alpine = { version = "1", args = { allow-apk = true } }` |
| `debian`       | `debian = { version = "1", args = { allow-apt = true } }` |
| `rust`         | `rust = { version = "1", args = { toolchain = "stable", allow-cargo = true } }` |
| `python`       | `python = { version = "1", args = { python-version = "latest", allow-pypi = true } }` |
| `nodejs`       | `nodejs = { version = "1", args = { node-version = "lts", allow-npm = true } }` |
| `docker`       | `docker = { version = "1", args = { allow-pulls = true } }` |
| `claude-code`  | `claude = { version = "1", args = { acp = false } }` |
| `openai-codex` | `codex = { version = "1", args = { acp = false } }` |
| `copilot-cli`  | `copilot = { version = "1" }` |

The args in the table are the defaults. See the page of each pack for
the other values.

`fedora`, `arch` and `suse` have no pack.
