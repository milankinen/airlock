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
the airlock vault with `airlock secret`:

```sh
airlock secret add MY_API_TOKEN
airlock secret ls
airlock secret rm MY_API_TOKEN
```

### Choosing a storage backend

The backend is picked with `vault.storage = "<backend>"` in
`~/.airlock/settings.toml`. The default is `file`, which makes the vault
work out of the box on any machine — including headless sessions where
a system keyring would try to pop a graphical unlock prompt.

| Backend          | At-rest protection                   | Prompts on use | Headless / CI friendly |
| ---------------- | ------------------------------------ | -------------- | ---------------------- |
| `file` (default) | `chmod 600` only (cleartext JSON)    | None           | Yes                    |
| `encrypted-file` | AEAD (ChaCha20-Poly1305 + Argon2id)  | Passphrase     | Yes (via env var)      |
| `keyring`        | OS keychain / Secret Service         | OS unlock      | GUI-dependent          |
| `disabled`       | N/A — `airlock secret` is turned off | None           | Yes                    |

```toml
# ~/.airlock/settings.toml
vault.storage = "encrypted-file"
```

Settings may also be written in JSON (`settings.json`) or YAML
(`settings.yaml` / `settings.yml`); TOML wins if more than one file
exists.

### `file` — plaintext JSON

Secrets and registry credentials are written to `~/.airlock/vault.json`
with mode `0600`. No crypto, no prompts, works everywhere — but anyone
who can read that file can read the secrets. `airlock secret add` shows
a one-time warning when this backend is active; pass `--yes` to skip
the confirmation in scripts.

### `encrypted-file` — passphrase-encrypted JSON

Same file, but the `data` field is an Argon2id-derived-key +
ChaCha20-Poly1305-encrypted blob. The passphrase is taken from the
`AIRLOCK_VAULT_PASSPHRASE` environment variable if set, otherwise
airlock prompts for it on the terminal. You'll be prompted twice on
first use (new vault) and once per process thereafter. The prompt line
is erased on successful input so the terminal stays clean.

For CI or non-interactive runs:

```sh
export AIRLOCK_VAULT_PASSPHRASE='correct horse battery staple'
airlock start
```

### `keyring` — system keychain / Secret Service

Stores the vault in the macOS Keychain or the Linux Secret Service.
First use triggers the OS unlock prompt. Well-suited to desktop use;
less suited to headless SSH sessions (where a graphical unlock can't
render) — use `encrypted-file` there instead.

### `disabled` — turn the vault off

`airlock secret` refuses to run. Registry auth falls back to
re-prompting on every 401 that needs credentials.

### Referencing vault entries

Vault entries are referenced with the same `${VAR}` syntax as host env
vars. The host env is consulted first and the vault is the fallback, so
common templates like `${PATH}` never open the vault — only names
the host env doesn't define fall through. If a shell var of the same
name already exists and you want the saved secret to win, unset it in
the invoking shell (or give the secret a different name).
