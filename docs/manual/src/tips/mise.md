# Pairing with mise

[mise](https://mise.jdx.dev/) is a polyglot tool manager and task runner.
It pairs well with airlock because it gives you a single place to define
project tooling, tasks, and environment variables — both on the host and
inside the sandbox.

## Installing airlock as a mise tool

Since airlock publishes GitHub releases, you can install it directly through
mise:

```toml
# mise.toml
[tools]
"github:milankinen/airlock" = "latest"
```

After `mise install`, the `airlock` binary is available in your PATH whenever
you're in the project directory. This makes onboarding straightforward —
new team members run `mise install` and get both the language toolchain and
the sandbox tool in one step.

## Building a local image with Docker

Rather than pulling a generic base image, you can build a project-specific
image with a Dockerfile and have airlock use it via the local Docker daemon.
This is handy when your sandbox needs tools or system packages that aren't
in the stock image.

Create a Dockerfile at the project root:

```dockerfile
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y \
    git curl build-essential
RUN curl https://mise.run | sh
RUN echo 'eval "$(~/.local/bin/mise activate bash)"' >> ~/.bashrc
```

Then add a mise task to build it:

```toml
# mise.toml
[tasks."build:image"]
description = "Build sandbox image"
run = "docker build -t myproject:dev -f dev.dockerfile ."
```

And point airlock at the local image:

```toml
# airlock.toml
[vm]
image = "myproject:dev"
```

With the default `resolution = "auto"`, airlock checks the local Docker
daemon first and finds your image there — no registry needed. You can
wrap the whole workflow into a single mise task that builds the image and
starts the sandbox:

```toml
[tasks.dev]
description = "Build image and start sandbox"
depends = ["build:image"]
raw = true
run = "exec airlock start --login"
```

## Loading secrets per task

airlock's `[env]` section can forward host environment variables into the
sandbox using `${VAR}` substitution. The question is where those host
variables come from.

mise supports a `mise.local.toml` file (gitignored by default) where you
can source secrets from a local script or set them directly:

```toml
# mise.local.toml — not committed
[env]
_.source = "~/.secrets/project-tokens.sh"
```

The sourced script can export whatever the sandbox needs:

```bash
# ~/.secrets/project-tokens.sh
export CLAUDE_CODE_OAUTH_TOKEN="sk-..."
export INTERNAL_API_KEY="key-..."
```

These variables are available to every mise task in the project. When a task
starts airlock, the sandbox config picks them up through `${VAR}` references:

```toml
# airlock.toml
[env]
INTERNAL_API_KEY = "${INTERNAL_API_KEY}"
```

This keeps secrets out of version control entirely — they live in a local
file on each developer's machine, loaded through mise, and forwarded into
the sandbox by airlock.
