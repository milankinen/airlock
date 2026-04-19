# Environment variables

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

## Variable substitution

To forward a value from the host into the sandbox, use the `${VAR}` syntax:

```toml
[env]
API_TOKEN = "${MY_API_TOKEN}"
```

When airlock starts, it resolves `MY_API_TOKEN` first from the host
environment and then from the [secret vault](../secrets.md), and injects
the result as `API_TOKEN` inside the container. Starting the sandbox
fails if the variable is not defined in either source.

You can provide a fallback value with `${VAR:default}`:

```toml
[env]
LOG_LEVEL = "${LOG_LEVEL:info}"
```

Substitution is handled by the [`subst`](https://github.com/fizyr/subst)
crate — see its docs for the full reference on escaping, nested
expansions, and other forms.

## Secrets

Values you don't want to keep in your shell environment can be saved in
the airlock secret vault and referenced by the same `${VAR}` syntax.
See the [Secrets management](../secrets.md) chapter for the full
reference — storage backends, trade-offs, and recommendations.
