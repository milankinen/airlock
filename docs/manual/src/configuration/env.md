# Environment Variables

The `[env]` section defines environment variables that are injected into the
container at startup. This is the primary mechanism for passing configuration
and secrets from the host into the sandbox.

## Static values

For values that are the same regardless of the host environment:

```toml
[env]
EDITOR = "vim"
TERM = "xterm-256color"
```

## Host variable substitution

More commonly, you'll want to forward a value from the host environment into
the sandbox. Use the `${VAR}` syntax:

```toml
[env]
API_TOKEN = "${MY_API_TOKEN}"
```

When airlock starts, it reads `MY_API_TOKEN` from the host environment and
injects it as `API_TOKEN` inside the container. If the host variable isn't
set, the value defaults to an empty string.

You can provide a fallback value with `${VAR:default}`:

```toml
[env]
LOG_LEVEL = "${LOG_LEVEL:info}"
```

