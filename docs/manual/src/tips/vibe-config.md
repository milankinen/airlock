# Vibe coding configuration

Airlock's configuration system is [hierarchical](../configuration.md#file-hierarchy).
That means you can put user-level settings in `~/.airlock/config.toml`
or `~/.airlock.toml`, and they will apply to every project sandbox by
default (and be overridden by per-project configuration where present).

This is especially handy if you want to "vibe code" and just point your
agent at a random directory without any extra setup. Since airlock can
build sandboxes from local Docker images, you can prebake one local
image with everything you need for your vibe-coding sessions.

Your `~/.airlock/config.toml` (or `~/.airlock.toml`) might look something
like this:

```toml
presets = ["debian", "rust", "claude-code"]

[vm.image]
name = "vibe:local"
resolution = "docker"

[network]
policy = "deny-by-default"
```

## Pairing with mise

If you're using [mise](https://mise.jdx.dev/) for your project tooling,
airlock pairs extremely well with it. You can use mise's task
dependencies and `sources` / `outputs` to build a local "vibe coding"
image and keep it up to date.

Create `~/.airlock/vibe.dockerfile`, for example:

```dockerfile
FROM debian:trixie-slim

ENV MISE_TRUSTED_CONFIG_PATHS="/"

# Install development dependencies
RUN apt-get update && apt-get install -y git curl build-essential
RUN curl https://mise.run | sh
RUN curl -fsSL https://claude.ai/install.sh | bash
RUN /root/.local/bin/mise use -g node@22

# Setup login shell
RUN echo 'export PATH=~/.local/bin:~/.cargo/bin:$PATH' >> ~/.bashrc && \
    echo 'eval "$(~/.local/bin/mise activate bash)"' >> ~/.bashrc && \
    echo '[[ -f ~/.bashrc ]] && source ~/.bashrc' >> ~/.bash_profile

ENTRYPOINT ["/bin/bash"]
```

Then add a user-level task in `~/.config/mise/config.toml`:

```toml 
[tasks."vibe:image"]
description = "Build vibe coding image"
quiet = true
hide = true
dir = "~/.airlock"
sources = ["~/.airlock/vibe.dockerfile"]
run = "docker build -t vibe:local -f vibe.dockerfile ."

[tasks.vibe]
depends = ["vibe:image"]
tools = { "github:milankinen/airlock" = "latest" }
description = "Start my vibe conding sandbox"
quiet = true
raw = true
dir = "{{ cwd }}"
run = "airlock start --monitor"
```

## Running your setup

Now `cd` into any project directory and run:

```bash 
mise vibe
```
