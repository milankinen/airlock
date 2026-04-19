# Application-wide settings + opt-in vault

Previously, any user who installed airlock and ran `airlock secret add`
or pulled from an authenticated private registry would get a system
keyring unlock prompt on first use. On macOS that's a modal Keychain
Access dialog; on Linux, a libsecret/GNOME Keyring unlock. Both are
surprising on first contact — the user didn't ask to put anything in
the keyring, they just ran a command.

We wanted the vault to stay a feature you turn on deliberately, while
leaving `airlock` usable with zero configuration.

## `Settings` struct in `crates/airlock/src/settings.rs`

Loaded once at `main` and threaded into subcommands that need it.
Candidate filenames under `~/.airlock/` tried in order:

1. `settings.toml`
2. `settings.json`
3. `settings.yaml`
4. `settings.yml`

First match wins; a missing file means all-default settings. A
malformed file fails loudly so the user notices instead of silently
falling back.

Fields use `#[serde(default, deny_unknown_fields)]`: every default
must keep `airlock` usable without a settings file, and typos in
field names are an error (not silently ignored).

For now there's just one field, `vault_enabled: bool` (default
`false`). New options get added here as they come up.

## Disabled vault via `NoopKeyring`

`Vault::disabled()` constructs a vault around a `NoopKeyring` whose
`load` returns `Ok(None)` and `store` drops writes. The rest of the
pipeline (`${VAR}` substitution, registry credential lookup, the
`secret` subcommand) doesn't need to know — it holds the same
`Vault` and all operations are safe. Substitution still works against
the host env snapshot; only secrets that would otherwise come from
the keyring go missing.

This avoids plumbing an "is-enabled" bool through every vault
accessor in the codebase.

## CLI behavior when disabled

`airlock secret {ls,add,rm}` refuses to run with a clear error
message pointing at `~/.airlock/settings.toml` and showing the flag
to set. We don't silently drop writes in the `secret` subcommand —
the user explicitly asked to manage vault state, so bailing out with
enable-instructions is less confusing than "command succeeded but
nothing happened."

Registry auth (`oci::credentials::load`/`save`) already degrades
gracefully on a failing keyring — it logs and treats the lookup as a
miss, so a 401 re-prompts instead of erroring. With `NoopKeyring`
that degradation path just takes effect permanently: every authed
pull prompts.

## Secret prompt: single entry, accept as-is

Previously `airlock secret add` double-prompted ("Value" + "Confirm"
with 3 retries on mismatch). In practice, people paste secrets from
password managers — a confirm round-trip doesn't catch anything and
just annoys. Dropped to a single `Value` prompt; `allow_empty_password(true)`
so empty input returns cleanly and the existing `is_empty()` check
bails with a proper error.
