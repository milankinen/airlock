# Presets

Presets are built-in configuration bundles that provide sensible defaults for
common ecosystems. Instead of manually listing every package registry and
cache directory for your tech stack, you pick the relevant presets and they
handle the details.

## Using presets

Add presets to the top-level `presets` array in your config:

```toml
presets = ["rust", "debian"]

[vm]
image = "ubuntu:24.04"
```

Presets are applied as a base layer — your own configuration always takes
priority and overrides anything a preset defines. Multiple presets can be
combined freely.

## Distribution presets

These open network access to the package repositories for each Linux
distribution, so that `apt install`, `apk add`, and friends work out of
the box.

- **`alpine`** — Alpine Linux package mirrors
- **`debian`** — Debian and Ubuntu package repositories (including PPAs and
  security updates)
- **`fedora`** — Fedora, CentOS, and RHEL package mirrors
- **`arch`** — Arch Linux and AUR repositories
- **`suse`** — openSUSE and SUSE update servers

Pick the one that matches your base image. If you're using `ubuntu:24.04`,
the `debian` preset is what you want.

## Language presets

These open network access to language-specific package registries so that
your package manager can fetch dependencies.

- **`rust`** — crates.io and Rust toolchain downloads
- **`python`** — PyPI
- **`nodejs`** — npm and Yarn registries

## AI agent presets

These configure network access, credentials, and file mounts for popular
AI coding agents. They handle the full setup: allowing the right API
endpoints, forwarding authentication tokens from the host, and mounting
settings directories into the sandbox.

- **`claude-code`** — Anthropic API access, OAuth token forwarding, and
  Claude settings
- **`copilot-cli`** — GitHub Copilot endpoints with path-level middleware
  restrictions, and GitHub CLI config
- **`codex`** — OpenAI API access and Codex settings

## Combining presets with custom rules

A typical project config combines a distribution preset with a language
preset and adds project-specific rules on top:

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

This gives you Debian package repos, PyPI, Claude API access, and your
internal API — all in a deny-by-default sandbox.

## Overriding preset rules

Since presets are just regular configuration applied at a lower priority,
you can override or disable any rule they define. If a preset opens network
access to something you don't need, disable it in your project config or
local overrides:

```toml
# airlock.local.toml
[network.rules.alpine-packages]
enabled = false
```

See the [Configuration](../configuration.md) chapter for more on how the
hierarchical config system works with `enabled` flags.
