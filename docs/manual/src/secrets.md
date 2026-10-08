# Secrets management

Most projects need secrets — API tokens, deploy keys, per-environment
passwords. The usual workaround is to export them as shell variables
and reference them from config with `${VAR}`. That approach is
inconvenient: you have to remember to export them every session. It is
also leaky: the value ends up in your shell history, in every child
process's environment, and often in log output. airlock ships a small
**secret vault** instead. You save a value once and reference it the
same way as any other `${VAR}`, but the value never appears in your
shell env.

airlock consults vault entries only as a fallback to the host
environment. Common templates like `${PATH}` still resolve from the
shell, and the vault stays closed. Only names the shell doesn't define
fall through to the vault.

## Quick start

Save, list, and remove secrets with the `airlock secrets` subcommand:

```sh
airlock secrets add MY_API_TOKEN     # prompts for the value
airlock secrets list                 # lists saved names + masked previews
airlock secrets remove MY_API_TOKEN
```

The short aliases `secret`, `ls`, and `rm` also work.

`list` prints a `VALUE` column with a `****`-prefixed preview. The
preview shows the last four chars of the value when it is at least 16
chars long, two chars when at least 8, and nothing for anything
shorter. Its only purpose is to tell apart several similarly-named
tokens. airlock never prints the full value anywhere.

Reference the saved value from `[env]` the same way as any host env
variable:

```toml
[env]
API_TOKEN = "${MY_API_TOKEN}"
```

On `airlock start`, airlock expands the template with the host env
first and the vault as fallback. It injects the result as `API_TOKEN`
inside the sandbox. The same substitution applies in Lua middleware
`env` tables (see [Network scripting](./advanced/network-scripting.md)).

## Choosing a storage backend

The vault supports four storage backends. Pick one with
`vault.storage` in `~/.airlock/settings.toml`:

| Backend             | At-rest protection                   | Prompts on use | Headless / CI friendly |
| ------------------- | ------------------------------------ | -------------- | ---------------------- |
| `keyring` (default) | OS keychain / Secret Service         | OS unlock      | GUI-dependent          |
| `encrypted-file`    | AEAD (ChaCha20-Poly1305 + Argon2id)  | Passphrase     | Yes (via env var)      |
| `file`              | `chmod 600` only (cleartext JSON)    | None           | Yes                    |
| `disabled`          | N/A — `airlock secrets` is turned off | None          | Yes                    |

```toml
# ~/.airlock/settings.toml
vault.storage = "encrypted-file"
```

You can also write settings in JSON (`settings.json`) or YAML
(`settings.yaml` / `settings.yml`). TOML wins if more than one file
exists.

### `keyring` — system keychain / Secret Service

Stores the vault in the macOS Keychain or the Linux Secret Service
(GNOME Keyring, KWallet). The first access per session triggers the OS
unlock prompt. After that, the keyring stays unlocked for the rest of
the session and no further prompts appear.

**Why it's the default**: on a normal desktop or laptop, the unlock
rides on your OS login. There is no extra passphrase to remember, and
secrets still get OS-level at-rest protection. The UX is
indistinguishable from any other app that uses the system password
store.

**Drawbacks**:
- On headless SSH sessions the graphical unlock can't render, so the
  first vault access hangs or fails. Use `encrypted-file` for
  CI / remote-development boxes.
- On Linux, the secret-service daemon has to be running. Minimal
  desktop setups and some WSL environments don't ship one.
- The vault is bound to the OS user account — a backup or a move to
  another machine isn't straightforward.

### `encrypted-file` — passphrase-encrypted JSON

Secrets live in `~/.airlock/vault.default.enc.json`, with the `data`
field as an Argon2id-derived-key + ChaCha20-Poly1305-encrypted blob.
The passphrase comes from `AIRLOCK_VAULT_PASSPHRASE` if set. Otherwise
airlock prompts on the terminal. airlock prompts twice on first use
(new vault) and once per process thereafter. It erases the prompt line
on successful input so the terminal stays clean.

**Why you might pick it**: it works on every platform, including
headless boxes where no keychain is available. In CI, supply the
passphrase via the environment variable:

```sh
export AIRLOCK_VAULT_PASSPHRASE='correct horse battery staple'
airlock start
```

**Drawbacks**: you have to type the passphrase once per shell session.
The protection is only as strong as the passphrase itself — a short or
reused one is a weak link.

### `file` — plaintext JSON

airlock writes secrets and registry credentials to
`~/.airlock/vault.default.json` with mode `0600`. There is no crypto
and there are no prompts. It works everywhere.

**Why you might pick it**: zero friction. Useful for throwaway test
boxes or when you're debugging the vault itself and need to inspect
the on-disk format.

**Drawbacks**: anyone who can read that file — including backup
snapshots, disk forensics, or a sloppy `tar` of your home directory —
reads the secrets. `airlock secrets add` shows a one-time warning when
this backend is active. Pass `--yes` to skip the confirmation in
scripts.

### `disabled` — vault turned off

`airlock secrets` refuses to run. `${VAR}` templates resolve only
against the host env, and if a referenced name isn't set there,
`airlock start` fails with a clear error. Registry auth re-prompts on
every 401 (airlock never saves the credentials). The [network services](./configuration/network.md#network-services)
are not available, so agents cannot sign in through airlock.

**Why you might pick it**: you already have a secrets pipeline you
trust (a 1Password CLI wrapper, a Vault agent, etc.) and you want
airlock to stay out of the way.

## When to switch away from the default

- **On a shared box, a CI runner, or a dev container** —
  `encrypted-file` with `AIRLOCK_VAULT_PASSPHRASE` supplied as a job
  secret. You get OS-independent at-rest protection without depending
  on a desktop keychain session.
- **For throwaway environments** — `file` is fine if you understand
  what you lose.
- **If you already manage secrets elsewhere** — `disabled`, and source
  the values into your shell env before running `airlock start`.

## Registry credentials

Credentials for private OCI registries also go through the vault. When
a pull gets a `401 Unauthorized`, airlock prompts for username and
password. If the vault is enabled, airlock saves them keyed by registry
host. Subsequent pulls from the same host reuse the saved credentials
without a prompt. With `disabled`, the pull still works but airlock
re-prompts on every `401`.

## Agent sign-ins

The [network services](./configuration/network.md#network-services) keep
the real sign-in tokens of the coding agents in `~/.airlock/db/`. airlock
encrypts them with a key from the vault. airlock makes the key when it
needs it the first time. `airlock secrets list` does not show it.
