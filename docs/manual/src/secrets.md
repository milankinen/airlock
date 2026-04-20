# Secrets management

Most projects need secrets — API tokens, deploy keys, per-environment
passwords. The usual workaround is to export them as shell variables
and reference them from config with `${VAR}`, but that's both
inconvenient (you have to remember to export them every session) and
leaky (the value ends up in your shell history, in every child
process's environment, and often in log output). airlock ships a small
**secret vault** so you can save a value once and reference it the
same way you would any other `${VAR}` — but without the value ever
appearing in your shell env.

Vault entries are consulted as a fallback to the host environment, so
common templates like `${PATH}` still resolve from the shell without
the vault ever being opened. Only names the shell doesn't define fall
through to the vault.

## Quick start

Save, list, and remove secrets with the `airlock secrets` subcommand:

```sh
airlock secrets add MY_API_TOKEN     # prompts for the value
airlock secrets list                 # lists saved names + masked previews
airlock secrets remove MY_API_TOKEN
```

The short aliases `secret`, `ls`, and `rm` also work.

`list` prints a `VALUE` column with a `****`-prefixed preview — the last
four chars of the value when it's at least 16 chars long, two chars when
at least 8, and no suffix at all for anything shorter. It's meant purely
for disambiguating entries when you have several similarly-named tokens
stored; the full value is never printed anywhere.

Reference the saved value from `[env]` the same way as any host env
variable:

```toml
[env]
API_TOKEN = "${MY_API_TOKEN}"
```

On `airlock start`, the template is expanded using the host env first
and the vault as fallback, and the result is injected as `API_TOKEN`
inside the sandbox. The same substitution applies in Lua middleware
`env` tables (see [Network scripting](./advanced/network-scripting.md)).

## Choosing a storage backend

The vault can be backed by one of four storage types, picked with
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

Settings may also be written in JSON (`settings.json`) or YAML
(`settings.yaml` / `settings.yml`); TOML wins if more than one file
exists.

### `keyring` — system keychain / Secret Service

Stores the vault in the macOS Keychain or the Linux Secret Service
(GNOME Keyring, KWallet). First access per session triggers the OS
unlock prompt; afterwards the keyring is unlocked for the rest of the
session and no further prompts appear.

**Why it's the default**: on a normal desktop / laptop the unlock
piggybacks on your OS login, so there's no extra passphrase to
remember and secrets still get OS-level at-rest protection. The UX is
indistinguishable from any other app that uses the system password
store.

**Drawbacks**:
- On headless SSH sessions the graphical unlock can't render, so the
  first vault access hangs or fails. Use `encrypted-file` for
  CI / remote-development boxes.
- On Linux, the secret-service daemon has to be running; minimal
  desktop setups and some WSL environments don't ship one.
- The vault is bound to the OS user account — backing it up or moving
  it between machines isn't straightforward.

### `encrypted-file` — passphrase-encrypted JSON

Secrets live in `~/.airlock/vault.default.enc.json`, with the `data`
field as an Argon2id-derived-key + ChaCha20-Poly1305-encrypted blob.
The passphrase is taken from `AIRLOCK_VAULT_PASSPHRASE` if set,
otherwise airlock prompts on the terminal. You'll be prompted twice on
first use (new vault) and once per process thereafter; the prompt line
is erased on successful input so the terminal stays clean.

**Why you might pick it**: works on every platform including headless
boxes where no keychain is available, and degrades cleanly in CI via
the environment variable:

```sh
export AIRLOCK_VAULT_PASSPHRASE='correct horse battery staple'
airlock start
```

**Drawbacks**: you have to type the passphrase once per shell session,
and the protection is only as strong as the passphrase itself — a
short or reused one is a weak link.

### `file` — plaintext JSON

Secrets and registry credentials are written to
`~/.airlock/vault.default.json` with mode `0600`. No crypto, no
prompts, works everywhere.

**Why you might pick it**: zero friction. Useful for throwaway test
boxes or when you're debugging the vault itself and need to inspect
the on-disk format.

**Drawbacks**: anyone who can read that file — including backup
snapshots, disk forensics, or a sloppy `tar` of your home directory —
reads the secrets. `airlock secrets add` shows a one-time warning when
this backend is active; pass `--yes` to skip the confirmation in
scripts.

### `disabled` — vault turned off

`airlock secrets` refuses to run. `${VAR}` templates resolve only
against the host env, and if a referenced name isn't set there,
`airlock start` fails with a clear error. Registry auth falls back to
re-prompting on every 401 (credentials are never saved).

**Why you might pick it**: you already have a secrets pipeline you
trust (a 1Password CLI wrapper, a Vault agent, etc.) and you want
airlock to stay out of the way.

## When to switch away from the default

- **On a shared box, a CI runner, or a dev container** —
  `encrypted-file` with `AIRLOCK_VAULT_PASSPHRASE` supplied as a job
  secret. You get OS-independent at-rest protection without depending
  on a desktop keychain session.
- **For throwaway environments** — `file` is fine if you understand
  what you're giving up.
- **If you already manage secrets elsewhere** — `disabled`, and source
  the values into your shell env before running `airlock start`.

## Registry credentials

Private OCI registries also store credentials through the vault. When
a pull gets a `401 Unauthorized`, airlock prompts for username and
password, and — if the vault is enabled — saves them keyed by registry
host. Subsequent pulls from the same host reuse the saved creds
without a prompt. With `disabled`, the pull still works but airlock
re-prompts on every `401`.
