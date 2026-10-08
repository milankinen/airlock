# Environment variables

The `[env]` section defines environment variables that airlock injects into
the container at startup. This is the primary mechanism for passing configuration
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

The [`subst`](https://github.com/fizyr/subst) crate handles substitution
— see its docs for the full reference on escaping, nested expansions,
and other forms.

## Secrets

You can save values you don't want in your shell environment to the
airlock secret vault, then reference them with the same `${VAR}` syntax.
See the [Secrets management](../secrets.md) chapter for the full
reference — storage backends, trade-offs, and recommendations.

## Masking

Set `mask = true` to keep a secret out of the sandbox:

```toml
[env]
API_TOKEN = { value = "${MY_API_TOKEN}", mask = true }
```

Inside the sandbox the variable holds a **surrogate**: an ASCII
alphanumeric string with the same number of bytes. airlock derives it
from the variable name and the length, never from the value, so it stays
the same on every start. The real value stays on the host. To use the
secret, list it in a network rule's
[`inject`](network.md#injecting-masked-secrets), which swaps the surrogate
for the real value in HTTP request headers.

- The table form accepts only `value`, `mask` and `optional`. Other keys
  are errors.
- airlock substitutes `value` first (`${VAR}` works as usual), then masks it.
- A later config layer that writes the plain string form only replaces the
  value — the entry stays masked. Set `mask = false` to unmask.
- Daemons and `airlock exec` see the surrogate too.

## Optional entries

Set `optional = true` to use a variable only if it has a value:

```toml
[env]
API_TOKEN = { value = "${MY_API_TOKEN}", mask = true, optional = true }
```

If the host environment and the secret vault do not define
`MY_API_TOKEN`, airlock does not set `API_TOKEN` in the sandbox, and the
start continues. An `inject` list skips the entry.
