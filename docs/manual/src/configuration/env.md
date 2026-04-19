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

## Variable substitution

To forward a value from the host into the sandbox, use the `${VAR}` syntax:

```toml
[env]
API_TOKEN = "${MY_API_TOKEN}"
```

When airlock starts, it resolves `MY_API_TOKEN` first from the host
environment and then from the user secret vault (see below), and injects
the result as `API_TOKEN` inside the container. Starting the sandbox
fails if the variable is not defined in either source.

You can provide a fallback value with `${VAR:default}`:

```toml
[env]
LOG_LEVEL = "${LOG_LEVEL:info}"
```

## Secret vault

Secrets that shouldn't live in your shell environment can be stored in
the system keyring with `airlock secret`:

```sh
airlock secret add MY_API_TOKEN   # prompts twice for the value
airlock secret ls                 # names and save times (values never shown)
airlock secret rm MY_API_TOKEN
```

The vault is **off by default** because the first access triggers a
system keyring unlock prompt that is surprising on first contact. To
enable it, create `~/.airlock/settings.toml` with:

```toml
vault_enabled = true
```

Settings may also be written in JSON (`settings.json`) or YAML
(`settings.yaml` / `settings.yml`); TOML wins if more than one file
exists. The same flag also controls whether OCI registry credentials
are persisted to the keyring after a successful login — with the vault
disabled, airlock re-prompts on every pull that requires auth.

Vault entries are referenced with the same `${VAR}` syntax as host env
vars. The host env is consulted first and the vault is the fallback, so
common templates like `${PATH}` never open the keyring — only names
the host env doesn't define fall through to the vault. If a shell var
of the same name already exists and you want the saved secret to win,
unset it in the invoking shell (or give the secret a different name).

