# Default vault backend is encrypted-file

The `file` backend was the default since the storage-backend work
landed. The motivation was that `file` is the only backend that works
unconditionally — no passphrase prompt, no keychain pop-up — so it
was chosen to keep the "first run just works" ergonomic. The code
review flagged this as inconsistent with what airlock stores in the
vault (registry credentials, user secrets flowing into sandboxes):
mode-0600 cleartext can leak via backups, Spotlight/Time Machine
indexers, and shared-home edge cases (devcontainers, NFS homes).

## Change

Move `#[default]` from `VaultStorageType::File` to
`VaultStorageType::EncryptedFile`. Update the module-header table and
the settings default-assertion test. Flip the opt-in example in
`docs/manual/src/configuration/env.md` from `encrypted-file` to `file`
so the TOML snippet now illustrates opting out of encryption rather
than opting in. Reorder the backend table so the default sits on top.

## Why encrypted-file and not keyring

`keyring` on Linux requires an unlocked Secret Service, which in
practice means a GNOME/KDE session; headless SSH can't render the
graphical unlock, so new users on servers/CI would hit an opaque
failure on first `airlock secret`. `encrypted-file` works everywhere:
the passphrase can come interactively or through
`AIRLOCK_VAULT_PASSPHRASE` for CI. It also keeps the vault file in
the user's home directory (backup-friendly, easy to inspect) without
leaking plaintext.

## Migration

`Vault::new()` is lazy — opening only happens on the first getter
call, so an existing `~/.airlock/vault.default.json` is not touched by
this change. Users who already have a plaintext vault will, on their
next `airlock secret` invocation, get a "no vault yet" state for the
new `encrypted-file` backend and have to recreate it, or explicitly
set `vault.storage = "file"` in `~/.airlock/settings.toml` to keep
using the old one. This is a behaviour change but an acceptable one:
the review treats the old default as a mild footgun, and callers who
want it back have a one-line opt-in.

## Not done

- No automatic migration path from `file` → `encrypted-file`. A
  one-shot `airlock vault migrate` could do it, but would require
  schema changes to unify the envelope formats. The current backends
  reject each other's envelopes, so no accidental cross-backend read
  can happen — the failure mode of a user hitting a stale pointer is
  a clear "no vault yet" instead of a corrupted-data panic.
- No first-run nudge pointing at `vault.storage = "file"`. The manual
  covers the trade-off; a CLI hint would duplicate that without
  adding clarity.
