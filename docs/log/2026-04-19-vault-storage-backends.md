# Pluggable vault storage: file, encrypted-file, keyring

The keyring-only vault made `airlock secret` and registry auth unusable
in the case the user spends most of their time in: headless SSH into a
machine with no graphical session. On Linux that's a libsecret prompt
that can't render; on macOS it's a Keychain modal that never appears.
Even when the user opted in via `vault_enabled = true`, the first
access just hung or failed, with no honest fallback that works headless.

## Three backends, one envelope

`Keyring` is renamed to `Storage`. Four implementations now plug into
the same `Vault`:

| Variant          | Where blob lives                | Encryption     |
| ---------------- | ------------------------------- | -------------- |
| `disabled`       | nowhere                         | —              |
| `file` (default) | `~/.airlock/vault.json`         | none, mode 0600|
| `encrypted-file` | `~/.airlock/vault.json`         | Argon2id + ChaCha20-Poly1305 |
| `keyring`        | OS keychain / Secret Service    | OS-managed     |

The two file-based backends share a path and use a tagged envelope:

```json
{ "type": "file",           "data": { ...VaultData... } }
{ "type": "encrypted-file", "data": { "kdf": {...}, "nonce": "...", "ciphertext": "..." } }
```

Each backend refuses to load the other's envelope — the file's `type`
tag is a load-time sanity check, not the selector. `vault.storage`
picks the backend; a mismatch errors loudly with a pointer at the
setting to flip. Silent reinterpretation was the main failure mode we
wanted to rule out: "I switched a toggle and all my secrets disappeared"
is worse than "I got a clear error and knew which toggle to flip back."

## Default changed to `file`

`settings.vault_enabled = false` (previous default) came from the
anti-surprise argument against keyring unlock prompts. That problem
goes away when the default backend is itself silent, so the default is
now `"file"` and zero-config installs just work — no prompt, no opt-in
step, no registry re-auth on every 401.

`chmod 600` is not "encryption at rest", but it is honest about what
it is. The `secret add` command confronts the user with a one-time
confirmation when the `file` backend is active, naming the two
stronger alternatives and their passphrase / OS-unlock cost. A
non-interactive shell (`--yes` absent, no TTY) fails closed rather
than silently writing cleartext, matching the existing `--stdin` rule
about never passing secrets on argv.

## Crypto choices for `encrypted-file`

Argon2id → ChaCha20-Poly1305, industry-standard pair:

- **KDF**: Argon2id, OWASP 2023 params m=19 MiB, t=2, p=1, 32-byte
  output. Lands around 100–300 ms on a modern laptop, which is the
  interactive-tolerable band without being trivially brute-forceable.
- **AEAD**: ChaCha20-Poly1305, RFC 7539. Fresh 12-byte nonce on every
  save — we rewrite the whole blob per mutation, so nonce reuse is a
  non-issue at this scale. AES-GCM would have been the equally-valid
  choice; ChaCha20 is marginally safer on hardware without AES-NI and
  the Rust crates are equally mature.
- **Salt**: 16 bytes, generated once per vault. Cached in memory for
  the process lifetime after first load or create, so repeated saves
  reuse the derived key and don't re-KDF. Rotating the salt across
  saves would have forced either caching the passphrase + re-KDFing
  (slow) or re-prompting on every write (user-hostile).

All binary fields are base64 (unpadded) in the JSON envelope so the
file is still text-editable for recovery.

## Passphrase handling

Pluggable `PassphraseSource` trait so tests inject a fixed passphrase
without a TTY. The production implementation:

1. `AIRLOCK_VAULT_PASSPHRASE` env var wins if set — escape hatch for
   CI, `systemd` units, Ansible runs, etc. Without this, headless
   `encrypted-file` would be unusable.
2. Otherwise prompts via `dialoguer::Password` with `.report(false)`,
   then erases the prompt line with `Term::clear_last_lines`. The
   terminal stays clean — no `"Passphrase: ****"` residue drifting
   into scrollback.
3. For fresh vault creation, `with_confirmation` double-prompts and
   two lines get cleared. For unlocking an existing vault, a single
   prompt with no confirmation.

No passphrase caching across processes. Each `airlock` invocation
prompts once; within a process, the derived key is cached so repeated
secret operations don't re-KDF.

## Why not `age`

`age` would have removed all KDF/AEAD handling from our side for ~one
extra dep. I prototyped it mentally and the two things that pushed me
back to hand-rolled primitives were (a) `age`'s file format carries
its own passphrase-derivation params that we can't easily surface in
the JSON envelope — we'd end up with an opaque base64 blob where I
wanted readable `kdf.*` fields for future migration; and (b) the set
of crates we pulled in (`argon2`, `chacha20poly1305`, `base64`, `rand`)
is tiny and all maintained by RustCrypto with the same review
pipeline. The DIY surface area is ~40 lines of direct primitive use,
which is well within what we can review and test.

## Atomic writes

File writes go through a sibling tempfile + fsync + rename. A crash
mid-write can't leave the vault truncated — either the old blob
stands, or the new one does. Mode 0600 is set at tempfile creation
time via `OpenOptionsExt::mode` so there's never a window where the
tempfile exists at 0644.

## Settings shape and parser

`vault_enabled: bool` → `vault.storage: VaultStorageType` enum is a
breaking change to `~/.airlock/settings.toml`. The nested `[vault]`
table leaves room for future knobs (passphrase-cache policy, custom
vault path) without polluting the top-level namespace. No migration
shim was added: the vault isn't in a released version yet, and a
stale `vault_enabled = true` line errors loudly as an unknown field
instead of silently degrading.

`Settings` now goes through the same `smart-config` pipeline as the
project-level `airlock.toml` loader — shared TOML/JSON/YAML
auto-detect, shared parse-error formatting — so the user sees one
consistent style of config error across both files. `VaultStorageType`
gets a `WellKnown` impl that deserializes from a string tag, matching
the `kebab-case` variants used everywhere else in the envelope and
docs.
