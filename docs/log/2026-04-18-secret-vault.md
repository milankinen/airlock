# Keyring-backed secret vault

Registry credentials and user secrets now share one keyring entry —
a single JSON blob under `(service="airlock", account="vault")` —
instead of the older per-OS pile (macOS Keychain entry per registry,
Linux JSON file under `~/.cache/airlock/`). `airlock secret add/ls/rm`
exposes the user-secret half to the CLI, and `[env]` templates pick
them up via the same `${NAME}` substitution that already read host env.

## Why one blob

The previous design had two problems:

1. **Two code paths.** Registry creds lived in the Keychain on macOS
   and a sidecar JSON file on Linux; user secrets didn't exist at all.
   Adding user secrets on top of that would have meant a third storage
   surface per platform.
2. **No authoritative listing.** `keyring` crate backends can
   enumerate entries only indirectly on some platforms, so any "list
   my secrets" feature would need a sidecar index — which can drift
   from real storage (the failure mode `rcman::list_keys` exhibits).

Collapsing everything into a single `VaultData` JSON blob sidesteps
both. `list_secrets()` reads one entry and iterates its `BTreeMap`; a
`set_*` or `remove_*` rewrites the whole blob. Registry creds and user
secrets are segregated by top-level key (`registries` vs `secrets`)
but share the same unlock prompt.

## Lazy opening

On Linux, touching the Secret Service bus prompts for keyring unlock.
That's unacceptable for commands like `airlock show` that don't need
secrets at all. So `Vault::new()` does no I/O; the first `get_*`/`set_*`
call opens (`keyring::Entry::get_password`, `serde_json::from_str`) and
caches the result behind a `Mutex<Option<VaultData>>`.

Substitution extends this: `VariableMap::get` on `Vault` consults the
host-env snapshot first and only falls through to `self.open()` when
env has no match. Templates like `${PATH}` or `${HOME}` — the hot
path — resolve without ever touching the keyring. Projects whose
`[env]` references only shell-provided vars never trigger an unlock.

## Substitution precedence

Three sites used to inline `subst::substitute(template, &host_env)`:
`vm::resolve_env`, `cli::cmd_exec::resolve_config_env`, and
`network/http/middleware::compile`. Each one built its own merged map
from host env + vault and each one had its own `templates_reference_vars`
guard to decide whether to touch the vault.

Now there's one `Vault::subst(template)` entry point with a fixed
precedence: host env first, vault as fallback. An earlier draft had
vault-wins — the reasoning being "an explicitly-saved secret is
probably what the user meant" — but that caused every `${VAR}` in
`[env]` to open the keyring and potentially prompt for unlock on
`airlock start`, even for plainly-shell-provided values like
`${PATH}`. The UX cost of repeatedly dismissing unlock dialogs
outweighs the footgun of a shell var shadowing a saved secret, which
users can easily fix by renaming or unsetting. The `VariableMap::get`
impl on `Vault` enforces the env-first ordering.

The three call sites now just call `project.vault.subst(template)`.
`substitution_env()` and `templates_reference_vars()` are deleted.

## Clonable `Vault` instead of `Arc<Vault>`

`Vault` holds its keyring backend + cached data in `Arc<VaultInner>`
and derives `Clone`. `main.rs` creates one handle and passes it by
value into each subcommand; clones share the same keyring backend and
cache. This is cheaper than the alternatives we considered:

- `&Vault` threaded through every callee — ergonomically bad since
  `Project` needs to own it long-term.
- `Arc<Vault>` explicitly — adds a pointer indirection layer the
  callers have to know about. Every site that took `Arc<Vault>` was
  already calling `&*vault` to reach the accessors.

Making the Arc internal means callers see a normal value type; tests
can construct a `Vault` without an `Arc::new` wrapper; and `Project`
stores `Vault` inline rather than `Arc<Vault>`.

## Env snapshot for testability

Earlier iterations of `VariableMap::get` called `std::env::var(key)`
directly. Tests for substitution would have to mutate the process env
(`std::env::set_var`), which is race-prone and platform-specific.

Instead, `Vault::new()` snapshots `std::env::vars()` into
`VaultInner.env` at construction. `VariableMap::get` reads from that
frozen map. `Vault::new_with(keyring, env)` lets tests inject a known
env without touching `std::env`, which is what the six `subst_*` unit
tests rely on.

The snapshot also makes substitution deterministic for the process
lifetime — nothing else can move variables out from under us mid-run.

## Keyring backend choice

On macOS, `keyring` uses `apple-native` (the modern Apple crypto
frameworks). On Linux, `sync-secret-service` + `crypto-rust` +
`vendored` — vendored OpenSSL avoids a system-libssl build dependency,
and `sync-secret-service` keeps us on D-Bus without pulling in tokio
inside the keyring crate.

## Migration

No migration path was written. The previous macOS `airlock-registry/<host>`
Keychain entries and Linux `registry-credentials.json` file are left
in place (users can delete them manually). The secret vault is a new
feature, so users who had registry creds saved will simply re-enter
them on the next registry 401 — the OCI retry loop has always handled
this case.

Skipping migration was a deliberate scope decision: this feature isn't
in a released version yet, so there are no production deployments
whose stored creds need preserving.
