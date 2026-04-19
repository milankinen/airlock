# Split vault.rs into vault/ submodule

`app/airlock-cli/src/vault.rs` grew to ~900 lines as four storage
backends (disabled, file, encrypted-file, keyring) accreted inside a
single file along with the `Vault` facade, envelope parsing, Argon2
constants, and tests. The code review called out the split as a
mechanical win — each backend is self-contained, the encrypted one
carries a meaningful chunk of crypto + UX that doesn't belong
interleaved with the plaintext path.

## Layout

```
src/vault.rs            # facade: Vault, OpenedVault, envelope types,
                        #         shared helpers, tests
src/vault/disabled.rs   # DisabledStorage
src/vault/file.rs       # FileStorage (plaintext JSON)
src/vault/encrypted.rs  # EncryptedFileStorage + PassphraseSource +
                        #   InteractivePassphrase + Argon2 derive
src/vault/keyring.rs    # KeyringStorage (Secret Service / macOS
                        #   Keychain)
```

## Visibility decisions

- Argon2 parameter constants and the shared serde envelope types
  (`Envelope`, `EncryptedBlob`, `KdfParams`, `VaultData`) stay in
  `vault.rs` and are exposed as `pub(crate)`. `file.rs` needs
  `Envelope` to reject encrypted-envelope files, and `encrypted.rs`
  needs all of the above plus the Argon2 constants. Keeping them in
  the parent module avoids sibling-to-sibling imports (which Rust
  forces through `super::sibling::Item`) and keeps the envelope
  shape in one place — the tag that distinguishes plaintext from
  encrypted is a cross-cutting invariant, not an encrypted-backend
  detail.
- `Storage`, `read_vault_file`, `atomic_write`, and `decode_b64_array`
  follow the same rule: `pub(crate)` in the facade, consumed via
  `use super::{...}` in backend siblings.
- The backend structs themselves (`DisabledStorage`, `FileStorage`,
  `EncryptedFileStorage`, `KeyringStorage`) don't need to escape the
  vault module — `boxed_storage()` in the facade is the only
  constructor — so they are `use`d, not `pub use`d.

## No behavior change

This is a pure refactor. All 18 existing `vault::tests::*` pass
unchanged. The on-disk envelope format, the Argon2 parameters, and
the backend selection path in `boxed_storage` are untouched.
