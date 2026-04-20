# Default vault backend flipped to keyring

Follow-up to [2026-04-19-vault-default-encrypted](./2026-04-19-vault-default-encrypted.md).
That entry picked `encrypted-file` as the default because `keyring`
fails opaquely on headless Linux without a running Secret Service.

## Why flip again

Defaults have outsized impact: in practice, nobody reads
`~/.airlock/settings.toml` before their first `airlock secret add`,
so whichever backend ships as the default is what the vast majority
of users will end up with. That's the lens this decision has to be
made through.

Through that lens, `encrypted-file` as the default is the wrong
trade-off:

- The passphrase is a new thing to remember, typed once per shell
  session. Friction on the common path.
- Strength is capped by whatever passphrase the user picks. A
  short or reused one buys very little at-rest protection over
  `file`.
- The target audience is mostly desktop / laptop users whose OS
  already has a keychain that piggybacks on the login session.

`keyring` on a normal desktop is strictly better: no second
passphrase, at-rest protection delegated to the OS vendor's
implementation, and the unlock happens transparently for apps the
user is already using. The headless case is the minority scenario
and has a clean opt-out — `vault.storage = "encrypted-file"` with
`AIRLOCK_VAULT_PASSPHRASE` for CI — which the manual now documents
prominently.

## Change

Move `#[default]` from `VaultStorageType::EncryptedFile` to
`VaultStorageType::Keyring`. Update:

- `app/airlock-cli/src/vault.rs` — enum `#[default]` marker +
  module-header table (list `keyring` first as default).
- `app/airlock-cli/src/settings.rs` — `VaultSettings::storage`
  doc comment + `missing_dir_yields_defaults` test assertion.
- `app/airlock-cli/src/cli/cmd_secret.rs` — reorder the "vault
  disabled, pick a backend" error message so `keyring` is on top
  and tagged as the default.
- `docs/manual/src/secrets.md` — reorder the comparison table,
  move the `keyring` section to the top with "Why it's the
  default" framing, demote `encrypted-file` to "when headless",
  and rewrite the Recommendation section into a
  "when to switch away from the default" list.

## Migration

Same as the previous flip: `Vault::new()` is lazy, so an existing
`~/.airlock/vault.default.enc.json` is not touched. Users with
a populated `encrypted-file` vault who do nothing will, on their
next `airlock secret` call, land on an empty keyring vault. Fix
is one line: `vault.storage = "encrypted-file"` in
`~/.airlock/settings.toml`.

This is acceptable: the user population that has an encrypted vault
today is tiny (default has been `encrypted-file` for one day), and
the failure mode is a "no secrets yet" state rather than a data
loss.
