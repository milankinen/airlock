# Installable packs, a setup wizard and agent sign-ins via the host

## Summary

- **Packs** replace config-only presets as the unit of reuse. A pack is
  a versioned folder `packs/<name>@<version>/` that ships with airlock:
  metadata and typed args (`pack.toml`), a config (static or `config.lua`)
  and an optional `setup.sh` that installs software into the sandbox.
  Projects enable packs in a `[packs]` table. The released list form
  `presets = [...]` still works and resolves exactly as before.
- **`airlock start`** gets a first-run setup wizard, decides what to do
  with an existing sandbox when its image or packs changed, installs packs
  in a separate, isolated install boot, and only then boots the run VM.
- **Network services** (`[network.services] anthropic|openai`) let Claude
  Code and Codex sign in inside the sandbox. The login page opens in the
  host browser. The real tokens stay encrypted on the host. The sandbox
  only gets surrogates.
- **Guest protocol**: `Supervisor.start` is replaced by `boot` (no
  process) and `spawn`. Every capability is built before the boot.
- Supporting changes: a process-wide `Context` (settings, vault, LMDB
  database `~/.airlock/db/`), symlink-safe host file access, a browser
  bridge, `airlock rm` deletes the whole project `.airlock/`, and
  `airlock show` lists packs and services.

Nothing here has been released. Several features that were prototyped
during development were removed before this commit and do not exist in
the tree: a separate `airlock vibe` command, an `airlock agents` command
with a sign-in VM, consent prompts, a `[tools]` table, a generated config
layer, and `@legacy` pack folders.

## Packs

### Format (`app/airlock-cli/src/packs.rs`, `packs/builtin.rs`)

A folder `packs/<name>@<version>/` at the repo root is embedded with
`include_dir`. `name` must match `[a-z][a-z0-9-]*` and `version` must be
a whole number from 1 with no leading zeros. The loader is strict: an
unknown file or a nested folder is an error. Dotfiles and `~` backups
are skipped.

- `pack.toml` (required): `label`, `description` (must not be empty),
  `kind` = `distro` | `agent` | `tool`, and `[[args]]`. All versions of a
  pack must have the same label and kind.
- At most one config file: `config.{toml,json,yaml,yml}` (static) or
  `config.lua`. A config must not set `packs` or `presets`.
- `setup.sh` (optional): POSIX sh. It must be idempotent and read its
  args only from the environment.

Packs are ordered by kind (distro, agent, tool), then by name. This
order applies to documents, installs and listings. `PackManager::resolve`
is async because remote packs are planned. Built-in packs today:
`alpine@1`, `debian@1` (distro); `claude@1`, `codex@1`, `copilot@1`
(agent); `docker@1`, `git@1`, `mise@1`, `nodejs@1`, `python@1`,
`rust@1` (tool).

**Args.** An arg is `bool` or `choice` (`values`, optionally `other =
true`, which accepts any non-empty string). Each arg has a typed default.
The keys `version`, `enabled` and `args` are reserved. Examples:
`nodejs` `node-version` (`lts`/`latest`/`none`/other), `python`
`python-version`, `rust` `toolchain`, `*-installs` bools that open the
package-registry hosts, and the `acp` bool on `claude` and `codex`.

**Versioning rule.** Versions copy their files and do not share them. A
change to a config or setup script ships as a new version. So name,
version and arg values together define what a pack puts on the disk.
This rule is by convention only: no test pins the content of a
published version. `lib.sh` is shared by all versions.

### Lua configs (`packs/lua_config.rs`)

`config.lua` runs on the host every time the config resolves (each
`airlock start` and `airlock show`). It can read host state, for
example `io.popen(...)`. Pack code is trusted because it ships with
airlock, so it gets the full standard library and no limits. This is
unlike the sandboxed Lua of network middleware. Globals:

- `config`: the table that the code fills.
- `pack.name`, `pack.version`, `pack.args` (defaults filled).
- `pack.directory`: `~/.cache/airlock/packs/mounts/<name>/`
  (`cache::pack_mounts_dir`), created before the code runs.
- `fail(msg)`: stops with the config error `pack <name>: <msg>`.

The agent packs keep the host side of their mounts in `pack.directory`.
`claude` mounts `.../claude/claude` at `~/.claude`, `codex` mounts
`.../codex/codex` at `~/.codex`, and `copilot` mounts `.../copilot/copilot`
at `~/.copilot`. So every sandbox that uses the pack shares one set of
agent settings and credential files.

### Install engine (`packs/install/`)

- **Installer** (`ConfiguredPack::setup_installer`, `compose.rs`): the
  script is a wrapper (`exec 3>&1 1>&2`), then `lib.sh`, then `setup.sh`,
  run as `/bin/sh -c` with argv0 `airlock-pack-<name>`. The env holds
  `AIRLOCK_PACK_API=1`, `AIRLOCK_PACK_ID`, and `AIRLOCK_PACK_ARG_<KEY>`
  (key upper-cased, `-` replaced by `_`, bools as `true`/`false`). Args
  are never secrets. Passing args through the env avoids a
  host-templated script, which would open an injection risk.
- **Fingerprint**: SHA-256 of the JSON of `(name, version, args)`. The
  scripts are not part of it (see the versioning rule). A different
  fingerprint on the disk is a "changed" pack, and only a re-created
  sandbox installs it.
- **Progress protocol v1** (`progress.rs`): the exec's stdout (fd 3 inside
  the script) carries only `steps <n>` and `status <text>` lines, written
  by `airlock_steps` and `airlock_status`. Every external command that
  `lib.sh` runs gets `3>&-`. Stderr is the log, written to
  `.airlock/sandbox/installs.log` (8 MiB cap, `[<pack>]` line prefix).
  Guest output is untrusted: it goes through `strip_controls`, status
  text is capped at 200 characters, step numbers stay within the
  declared count (max 99), and the label comes from the host. No
  decision depends on the status text, so a spoofed status has no
  security effect. A typed RPC would be needed before third-party packs.
- **State** (`state.rs`): `.airlock/sandbox/installs.json`, versioned,
  max 256 KiB, every field validated on read. It records the disk id
  (a random id per new disk image), the image id, and one record per
  pack: `unconfirmed` | `installed` | `failed` | `kept{confirmed}` with
  fingerprint and time. Exit 0 gives `unconfirmed`. Only a shutdown with
  a confirmed guest disk sync promotes those records to `installed`.
  The state is saved after each pack, so after a crash the next start
  knows what to retry. A state file written by a newer airlock gives
  exit 2. A corrupt state file is treated as empty in a terminal and
  gives exit 2 without one.
- **Plan** (`plan.rs`, pure): compares the configured packs with the
  records, the disk id and the image id. The decision table is in the
  doc comment of `decide`. An `unconfirmed` or `failed` record with the
  same fingerprint is a `Retry` and installs without a question.
- **Install boot** (`setup.rs`, `phase.rs`): one VM, one `spawn` per
  pack. Packs are independent: a failed pack does not stop the next one.
  There is a 20 min idle timeout and a TERM/KILL grace period. The
  install config is narrowed from the resolved config: policy
  `allow-always`, every rule disabled with its inject list cleared, plus
  one rule `airlock-install` with `allow = ["*"]` and passthrough (no TLS
  interception). It gets no services, middleware, secrets or masked env
  (only an unmasked `HOME`), mounts, project share, ports, sockets,
  daemons, masks or clipboard.
- **Image check** (`facts.rs`): before anything installs, the image must
  run as uid 0 and have an `/etc/os-release` with `alpine`, `debian` or
  `ubuntu` in `ID`/`ID_LIKE`. This check runs on the host from the image
  layers.
- **`lib.sh`**: distro, arch and libc detection; `pkg_install`
  (apk/apt); `fetch` (curl, https only, TLS 1.2+); `run_vendor`;
  `github_latest_tag`. Exit codes: 10 unsupported distro/CPU, 11 package,
  12 download/vendor, 13 bad arg value. `exit_hint` maps them to user
  hints. A scratch directory on the sandbox disk (not the `/tmp` tmpfs,
  because some downloads are large) is removed at exit.
- **Pack setup scripts** use the official vendor installers or release
  builds, not a mise backend. They do not pin checksums themselves where
  the vendor installer already verifies its downloads (claude, codex,
  mise, nvm, uv, rustup). `copilot` checks `SHA256SUMS.txt` itself.
  `docker` and `git` install distro packages. The agents install to
  `/usr/local/bin`, which the sandbox user cannot change. So self-updates
  are disabled (`DISABLE_AUTOUPDATER`, `COPILOT_AUTO_UPDATE=false`), and
  a newer agent comes with a new sandbox.

### ACP adapters (`acp` arg of `claude@1` / `codex@1`)

- `bun_compile <package> <command> <file>` (in `lib.sh`) downloads Bun
  from the latest `oven-sh/bun` release into scratch space. The zip
  matches arch and libc, and the x86_64 builds are `baseline` so the
  compiled file runs on CPUs without AVX2. The zip is checked against
  `SHASUMS256.txt`. The helper installs the npm package with
  `bun add --omit=optional --ignore-scripts` and builds one root-owned
  0755 executable with `bun build --compile`. Then it removes Bun, the
  scratch project and the Bun cache, also on failure. Only the
  executable stays, about 80 MB, which is mostly the Bun runtime.
- The compiled adapters have no agent binary of their own: the package
  resolves it with `require.resolve` at runtime, and optional
  dependencies are not installed. The pack's `config.lua` therefore sets
  `CLAUDE_CODE_EXECUTABLE=/usr/local/bin/claude` or
  `CODEX_PATH=/usr/local/bin/codex` when `acp` is true. As a result, the
  adapters run the pack's own agent, which can be newer than the version
  that the adapter declares.
- The compiled entry drops a duplicated `argv[1]` (the `/$bunfs/root/...`
  path). Without this, claude-agent-acp's terminal login passed that path
  to `claude` as a prompt.
- With `acp` off, `acp_stub` writes a script at the adapter path that
  prints `airlock acp support for <agent> is not enabled` and exits 1.
  `bun_compile` rebuilds whenever the file is a script.

## Config

### Layers (`config.rs`, `config/files.rs`)

Lowest precedence first: `~/.airlock/airlock.*` < `~/.airlock/config.*`
< `~/.airlock.*` (user files) < `<project>/.airlock/airlock.*` (local
project file, git-ignored) < `<project>/airlock.*` <
`<project>/airlock.local.*`. In each slot the first existing extension
wins, in the order toml, json, yaml, yml. When the project root is
`$HOME`, `.airlock/airlock.*` counts only as the local project file.

`config::load() -> LayeredConfig` and
`resolve(&PackManager, &ConfigOverrides) -> ResolvedConfig { values,
packs }` are immutable. `--network` is a `ConfigOverrides` applied
inside `resolve`. The wizard's output becomes the project layer through
`with_generated_project(self, ..)`. `ResolvedConfig::install_config()`
gives the narrowed install config.

### `[packs]` entries (`config/pack_entries.rs`)

`<name> = { version = "1", enabled = false, args = { ... } }`, where
every key is optional per file:

- **Only project-level files** (local project file and project files)
  can have `[packs]`. In a user file it is an error. Reason: a pack in a
  user file would install software into every project, and the wizard
  would have to pre-select from it.
- The entries of all files merge per pack. `enabled` (default true) and
  `version` come from the highest file that sets them. `version` is
  required on the merged entry (a string, or an integer ≥ 1). Args come
  only from files whose entry has the final version or no version; per
  arg the highest file wins, and each value is type-checked. Errors name
  every file involved. An unknown pack whose name is a released list name
  gets a hint to use the list form. Version `"legacy"` is refused with
  the same hint.
- **Merge order** in `resolve`: the documents of all `presets` lists
  first (beneath everything, as released), then for each layer the
  config values of the packs whose highest entry is in that layer,
  followed by that layer's own values. So a pack overrides the layers
  below its entry, and the file that holds the entry overrides the pack.
- **Conflict detection** (`merge::pack_conflicts`): the config values of
  the enabled packs must not set the same scalar path to different
  values. Objects merge key by key, arrays concatenate, and null counts
  as no value. Env entries are compared per field after
  `normalize_env`. Each conflict is a config error such as ``packs nodejs
  and python both set `env.FOO.value` ("a" vs "b")``. Config files are
  not checked against packs: they override packs by design.

### Released list form (`config/legacy_presets.rs`)

`presets = ["python", ...]` is config only and never installs anything.
Only the 12 names whose files are in `src/config/presets/*.toml` are
accepted, byte-for-byte as released (`arch`, `fedora` and `suse` have no
pack). A pack name that is not a released name gives an error with a
`[packs]` hint. There are deliberate breaks: a `presets` value that is
not a list of strings is an error, `presets: null` included. Leftover
unknown tables are ignored, because smart-config does not reject unknown
keys.

**Golden oracle**: `config/tests/test_legacy_presets.rs` +
`config/tests/golden/legacy-presets.json` (sha256 `ca4aabfc...`) pin the
resolved config of legacy configs. Regenerate only on purpose with the
ignored test `regenerate_legacy_presets_golden`.

### `[network.services]`

`BTreeMap<String, bool>`, off by default. An unknown name is a config
error. The map is left out of serialization when empty, so the golden
file stays unchanged. A passthrough rule that covers a service host is a
conflict (`network.rs`, labeled ``service `<id>` host `<host:port>` ``).

## `airlock start` flow (`cli/cmd_start.rs`, `start/`)

```
check_system_requirements → host cwd → init_logging (.airlock/airlock.log)
packs::init → wizard::load_or_generate_config → config.resolve
HostRuntime::new → sandbox::ensure_sandbox (questions, lock, image, disk)
wizard::save_config (only now) → project::open(&lock, values, ..)
install::install_tools → run::run_sandbox (run_interactive)
```

- **Wizard** (`start/wizard*.rs`) runs only when the project has no
  project config, no sandbox, and a terminal is attached. A sandbox
  without config, or no terminal, gives exit 2. It is a single view:
  distro radio group (or `custom (<image>)` when a user file sets
  `vm.image`), agent and tool checkboxes with nothing pre-selected, arg
  rows for each selected pack, clipboard copy/paste checkboxes, and a
  start bar with `start` (writes `.airlock/airlock.toml`), `start and
  share` (writes `airlock.toml`) or `cancel`. Before it accepts, the
  answers must resolve together with the user files, including `[env]`.
  The file is written with create-new only after the sandbox is stored,
  so a failed or cancelled start leaves no config behind. A file that
  appears in the meantime is an error and is not overwritten.
- **`ensure_sandbox`** (`start/sandbox.rs`) runs the early `[env]` check,
  takes the sandbox lock and reads `installs.json`. There are four
  questions, and `re-create sandbox` is the default for each: image
  changed (re-create / continue with current / cancel); packs changed
  (re-create / cancel); tools removed (re-create / continue, which
  records them as `kept`); tools added (re-create / install anyways /
  cancel). The added-tools note warns that the install runs with an open
  network inside the existing sandbox, where code already on the disk
  can run. `--yes` always picks the default. Without a terminal and
  without `--yes`, a needed question gives exit 2 before the image pull.
  A new disk, an unchanged sandbox and retries need no question.
- **Config is the source of truth for tools**: there is no "start
  without installing". `install_tools` exits 1 if a configured pack still
  has no `installed` record. The install boot uses its own `Project`
  (`Project::with_config`); the run project is never mutated. The lock
  is held by `cmd_start::run` for the whole run, not by `Project`.

### Boot sequence (`sandbox/`)

`run_interactive` builds everything the guest gets before the VM starts:
`services::build_enabled` → `Browser::new(services.browser_grants())`
(`None` without grants) → `Network::new(.., interceptors,
denied_targets)` → pure `guest_env` (adds the browser shim) → daemon and
mask specs → `clipboard::for_config` → `boot(BootSpec)`. Then
`services.attach(guest_network)` → `vm.spawn(main)` → drive →
`services.detach()` → `vm.shutdown()`. `boot` starts no process. A
failed `Supervisor.boot` shuts the VM down and returns an error, so
`airlock start` exits 1 (previously 100 through a fake main process).
Each boot owns its background tasks (`sandbox/tasks.rs`), and they stop
before the next boot in the same process starts (install boot, then run
boot). `airlock exec` calls `Supervisor::spawn` with the sandbox env
from `Vm::serve_cli`.

## Network services and agent sign-ins (`src/services/`)

The module docs of `services.rs`, `oauth.rs`, `anthropic.rs`,
`openai.rs`, `store.rs`, `auth_codes.rs`, `callback.rs` and `sign_in.rs`
are the detailed reference. Key points:

- **Why in-sandbox sign-in**: the agents' own logins (`claude /login`,
  `claude setup-token`, `codex login`, `codex login --device-auth`) give
  full sign-ins. A host-side command could only offer setup tokens or
  pasted keys, and it briefly held real tokens in a helper VM.
- **Owned hosts**: `anthropic` owns `platform.claude.com` and
  `api.anthropic.com`. `openai` owns `auth.openai.com` and `chatgpt.com`
  (token swap only under canonical `/backend-api/` paths, WebSocket
  included). `Network::resolve_target` allows an owned host unless the
  policy is `deny-always` or a deny rule matches. An owned host is
  always intercepted and never passthrough. Interception happens only on
  TLS connections. The interceptor seam is `network/interceptor.rs`, and
  the network layer has no knowledge of OAuth.
- **Surrogates**: random strings in the provider's token format.
  Anthropic: `sk-ant-oat01-airlock-...`, `sk-ant-ort01-airlock-...`, and
  `sk-ant-api03-airlock-...` for keys from `create_api_key` (max 3
  creations per hour per grant, max 8 kept). OpenAI: unsigned fake JWTs
  with the real claims and the real `exp` (Codex checks no signature),
  plus `airlock-rt-...`. The swap happens only on exact matches in the
  specific header on API paths. `pin_authority` forces Host/`:authority`
  to the endpoint. Monitor events and Lua middleware run before the
  interceptor, so they see surrogates only.
- **Strict credentials** on API paths: a bearer or `x-api-key` must be a
  service surrogate or the real value of an injected masked `[env]`
  secret. Anything else gets a local `401` (`foreign_credential`). This
  stops one sandbox from planting its own token in the shared credential
  file and having other sandboxes use it. Codex API-key users use a
  masked `OPENAI_API_KEY` with `inject`.
- **Fail-closed token endpoints**: no query, JSON or form body without
  duplicate keys, a grant type from an allowlist, and the path matched
  after normalization on its own host. The upstream body is re-serialized
  from the parsed fields. A 2xx answer that is not uncompressed JSON with
  the expected fields gives a local `502`. The `backstop` refuses other
  JSON answers of 64 KiB or less on owned non-API paths when they carry
  a token or code field, or a provider-format token. The `api_backstop`
  refuses small uncompressed API answers that contain a real
  `sk-ant-*` token. Requests on owned hosts that the service does not
  recognize go through the backstop and are never forwarded raw.
- **Authorization codes** (`auth_codes.rs`): the callback forward and the
  Codex device poll swap the real code for `airlock-code-...`. The code
  is bound to the service and the channel (callback port or device
  flow), works once, lives 10 min, and each service keeps at most 32 in
  process memory. An exchange with an unknown code gets a local
  `invalid_grant`.
- **Browser bridge**: the guest shim `/run/airlock/bin/xdg-open` writes to
  a FIFO, and airlockd calls `Browser.open`. `$BROWSER` points at the shim
  and its dir goes first on `PATH`, unless the user's `[env]` sets
  `BROWSER`. The host `Browser` (`rpc/browser.rs`, provider-agnostic)
  applies a rate limit of 5 opens per minute, then URL hygiene (https, no
  userinfo, port or fragment, length and character limits), then asks the
  grants (`GrantAnswer::{NotMine, Allow, Refuse(reason)}`). Notices are
  printed after the session so they do not corrupt a full-screen TUI.
  Logs name only the host and path, never the query, because it carries
  OAuth state.
- **Sign-in grant and callback** (`sign_in.rs`, `callback.rs`):
  `LoopbackSignIn` accepts only the service's pages with the expected
  `client_id`, `response_type=code`, S256, known scopes (each once), no
  `prompt=none`/`response_mode`, and exactly one loopback `redirect_uri`
  on an allowed port and path. Anthropic allows ports 32768-60999 and
  OpenAI 1455 and 1457. The port is bound exclusively on the host, so
  the guest never gets traffic meant for a host program. Each service has
  one forward at a time, and a new port replaces it. The forward accepts
  only `GET`, strips `Cookie`/`Authorization` in and
  `Set-Cookie`/`Refresh` out, adds a CSP `sandbox` header, allows
  redirects only to the same loopback origin or an https page of the
  service, and closes 1 min after the request with the code.
- **Refresh**: the agent owns the token lifecycle, as on a host. The
  proxy only relays the agent's `refresh_token` request
  (`Grants::relay_refresh`). It looks up the grant, reads the real
  refresh token uncached, posts upstream in a detached task (a dropped
  guest request cannot lose rotated tokens), and stores the result with
  `TokenStore::replace_tokens` in one LMDB write transaction on the
  current record. The guest gets surrogates with the upstream
  `expires_in`. Refresh surrogates and the Anthropic access surrogate
  stay stable. OpenAI access and id_token fake JWTs are re-minted with
  the new `exp`, and up to 4 previous access surrogates keep working
  until their own `exp`. Upstream 401s pass through to the agent.
  Concurrent refreshes of one grant are last-write-wins, as on a host.
  The store only guarantees atomic writes.
- **Logout is global**: a revoke with either surrogate deletes the grant
  for all sandboxes and revokes the real tokens upstream (detached). A
  replaced grant is revoked too. A refresh that finishes after a sign-out
  revokes its new tokens. `codex login` revokes before each login. A new
  sign-in replaces older grants with the same service, account and
  scopes.
- **Store** (`store.rs`): the databases `services.grants` and
  `services.lookups` in `~/.airlock/db/`. The secret part of a grant (real
  tokens, surrogates, created keys) is sealed with ChaCha20-Poly1305 (AAD
  = grant id + service) under keys derived from a 32-byte vault field
  `service_store_key`. This field is created once under the vault lock
  and is not a listed secret. Lookups are keyed by HMAC-SHA256 of the
  surrogate. Plain fields (service, account label, scopes, timestamps)
  let `airlock show` list sign-ins without the key. Each process caches
  decrypted grants for at most 10 s, and an entry whose access token has
  expired is read again.
- **Fail closed when unavailable**: if the vault is `disabled` or the key
  cannot be read, the enabled services are unavailable. Their hosts are
  then denied under every policy, and one warning per process names the
  opt-out `[network.services] <name> = false`. Otherwise the agent could
  sign in natively and keep real tokens in the sandbox.

## Other changes

- **`Context`** (`context.rs`): settings, vault and `Db`, built once in
  `main` and held by `Project`. **`Db`** (`db.rs`): one `heed`/LMDB
  environment per process at `~/.airlock/db/` (dir 0700). It has named
  databases `<purpose>.<name>`, and every transaction runs whole in
  `spawn_blocking`. LMDB was chosen because writers serialize across
  processes without `Busy` errors. turso locked the file or returned
  `Busy` on open with multiple processes, and the pure-Rust KV stores
  lock their file to one process. A database that cannot open stops
  every command with exit 1.
- **Vault**: every persistent backend does read-modify-write under a
  cross-process lock file, and the keyring backend now has one too
  (`vault.keyring.lock`). Unknown top-level fields survive writes, except
  the retired `agents` section (real tokens from unreleased builds),
  which the next write drops. The prompt UI moved to `vault/ui.rs`.
- **`.airlock/`** is created 0700 through `util/safe_fs.rs`
  (`PinnedDir`: `*at()` + `O_NOFOLLOW`, regular files owned by the
  user). A symlinked or foreign-owned `.airlock` is refused. `PinnedDir`
  also protects host access to rw-mounted pack directories where the
  guest can plant symlinks (`vm/file_sync.rs` now uses it).
- **`airlock rm`** (`cli/cmd_rm.rs`) removes the whole project
  `.airlock/`, including `.airlock/airlock.*`. The prompt and success
  message say so. It removes only `.airlock/sandbox` when the project is
  a home directory (`$HOME`, or the passwd home of the current user or of
  the owner of `.airlock`), or when `.airlock` holds user-level files
  (vault files, `db/`, `claude/`, `codex/`, `agents/`, `settings.*`,
  `config.*`). It holds the sandbox lock (`project::lock_if_idle`) during
  the removal and does not load the config.
- **`airlock show`** lists the enabled packs with non-default args and a
  status (`installed`, `pending`, `config only`, `kept, removed from
  config`, ...) and has a `Services:` section (sign-ins, `not signed in`
  or `unavailable (...)`). It exits 2 on config errors.
- **Prompts** (`cli/prompt/`): generic raw-mode widgets (choose, yes/no,
  fields, screen) used by the wizard, the questions and `secrets add`.
- **Guest (`airlockd`)**: `boot`/`spawn` (`rpc.rs`). A second `boot` is
  refused, `spawn` before a successful boot is refused, and boot errors
  are returned as RPC errors. PID 1 idles after boot. Pipe-mode children
  lead their own process group, so host signals reach their children.
  Spawned processes get only the given env. A per-boot tmpfs at
  `/run/airlock` holds the bridge FIFOs and shims, so a shim cannot
  persist into later boots that have no grant. A symlink at `run` or
  `run/airlock` skips the tmpfs with a warning. `bridge.rs` is the shared
  FIFO toolkit, and `browser.rs` is the browser bridge. The
  `supervisor.capnp` schema adds `Browser`/`BrowserConfig`, where a null
  sink means not granted.
- `DenyReporter` sends deny reports from a task owned by the boot and
  coalesces them through a `watch` channel.

## Security notes

- The sandbox is untrusted. It never receives a real provider token or
  authorization code (except in the manual Claude paste flow, see
  below).
- The install boot deliberately has an open network: `allow-always` with
  passthrough. It can reach host loopback, the LAN and cloud metadata. A
  name-based loopback deny was rejected because matching uses the
  guest-supplied name while DNS resolves on the host, so it is not a
  boundary. Code already on the persistent disk can run during an
  install into an existing sandbox. The added-tools question says so,
  and re-create is the default.
- `config.lua` is trusted host code. Only built-in packs exist. Remote
  or third-party packs would need a sandboxed Lua state and a typed
  progress RPC.
- Agent credential files with surrogates are shared through rw mounts
  between sandboxes, and strict credentials stop cross-sandbox token
  planting.

## Known limits and not verified

- **Not run end to end** (no KVM on the dev machine): real `claude
  /login`, `claude setup-token`, `codex login`, `codex login
  --device-auth` through the services (browser bridge, code swap,
  refresh relay, global logout, provider success pages against the
  redirect rules), `tests/vm/packs.bats`, and the KVM-guarded cases of
  `tests/cli/start.bats`.
- **Unverified provider facts**: protocol facts come from reading Claude
  Code 2.1.288 and Codex 0.158.0. It is unknown which credential Claude
  sends to `mcp-proxy.anthropic.com`, which is not owned, so a surrogate
  sent there stays a surrogate. OpenAI access-token revoke and whether
  `codex logout` revokes are also unknown.
- Claude's manual code-paste flow (`redirect_uri`
  `https://platform.claude.com/oauth/code/callback`) brings the real code
  into the sandbox through the clipboard. Its exchange is forwarded with
  that code.
- ACP adapters were built and run with `lib.sh` only on a Debian aarch64
  host (no VM). musl, x86_64 and an in-VM install are untested.
- Missing tests: the `with_generated_project`, `ConfigOverrides` and
  `Project::with_config` paths, `replace_tokens`, a dropped refresh
  request, a refresh racing a sign-out, old OpenAI access surrogates, the
  expired-cache re-read, guest `spawn` before `boot`, browser grant
  ordering and refusal reasons, and that sign-in refusals never echo the
  query.
- `airlock show` never opens the vault, so it cannot detect a failing
  vault key. There is no CLI to remove a sign-in; a logout inside a
  sandbox does it. The callback forward stays bound after its 1 min
  window until the session ends.
- A pack version's files are immutable by convention only. Changing an
  existing version's `setup.sh` or the shared `lib.sh` does not change
  fingerprints.
- The user manual is not updated for packs, the setup wizard or the
  network services; it will be rewritten separately.
