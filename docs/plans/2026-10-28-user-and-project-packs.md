# User packs (`~name`) and project packs (`./path`)

## Context

All packs come from the binary today. Users need their own packs to
replace user-level config files, which will be deprecated. Projects need
packs in the repo for shared project setup. Both use the built-in pack
layout. Later, GitHub packs (`owner/repo:name` + `ref`) use the same
identity model.

Non-built-in pack code is not trusted until the user approves it. The
guest can write the project dir, and a teammate's pull can change a
pack. So airlock asks before it runs the `config.lua` of a new or
changed pack on the host.

Gaps in current code:
- `PackManager` holds only built-ins. Data is `&'static`.
- `config.lua` runs inside `LayeredConfig::resolve` with no check.
- The install fingerprint (name, version, non-default args) ignores
  content.
- Changed packs give only "Re-create".
- Bug: a sandbox seeded from a template counts as a new disk, so changed
  packs silently stay old (`plan.changed`, `NoQuestion`).

## Decisions (user)

| Topic | Decision |
|---|---|
| Location | Settings `[packs] dirs = ["~/.airlock/packs"]`, `defaults = []` (wizard pre-select, built-in and `~` names) |
| Layout | `<dir>/<name>/{pack.toml, config.<ext>, setup.sh?}`, no `@N` |
| User packs | `"~name"` in `[packs]`. No `version` (error if set). Future: `ref` for GitHub |
| Project packs | `"./packs/foo"` in `[packs]`, relative to the project root, must start with `./`. No `version` |
| Errors | Same `~name` in two dirs. Missing referenced pack, also in `config --resolved` and the change check |
| Kind | Optional. New `other` (built-ins too). Missing = `other`. Wizard "Other" section |
| Wizard | Marker `(local)` for `~` packs. Writes all packs to `airlock.toml` |
| Approval | New or changed `~` and `./` packs need approval **before** their Lua runs. Approval is keyed by content hash. A wizard selection counts as approval. `--yes` never approves. Built-ins need no approval |
| Setup data | `config.lua` passes env to `setup.sh`. Values are secret: never logged or shown, only changed key names |
| Changed pack (disk) | Show file diff + changed setup keys. Options: Re-create (default, `--yes`) / Reinstall on top / Keep old. Existing sandboxes and template seeds |
| Snapshots | Sandbox and template store copies of the installed pack files for the diff |
| lib.sh | Stable `airlock_*`: log, fail, detect_distro, detect_arch, detect_libc, pkg_is_installed, pkg_install, fetch, github_latest_tag, run_vendor |
| Deprecation | Warning on `airlock start` if a user-level config file exists |

### Changes after the implementation review

These decisions replace the parts of the design below that differ.

- Approval applies only to `~` and `./` packs that have a `config.lua`.
  The approval hash covers only the `config.lua` text. Packs with a
  static config or no config need no approval.
- Approval question for new packs: "Sandbox contains packs that may
  execute code on your computer:", the pack list, Abort / Continue.
  For changed packs: "The following sandbox packs have changed and need
  approval:", "(press s to show changes)", the pack list (new packs
  marked "(new)"), Abort / Approve. `s` prints the full diff above the
  question. Nothing prints before the question.
- The changed-pack question of the disk uses the same compact form
  with `s`. The "already shown in this run" mechanism is removed.
- `airlock start --auto-approve-packs` approves new or changed packs
  with no question and prints their ids. It also works with `--yes` and
  with no terminal.
- `./` packs are allowed in all config files that can have packs
  (user files still refuse all packs). `LayerKind` is removed. Template
  builds refuse `./` packs.
- The disk reader reads only the known pack files (`pack.toml`,
  `config.<ext>`, `setup.sh`) and ignores other entries. The total pack
  size limit is removed.
- `lib.sh` also makes `airlock_pkg_available` public.
- A `setup.sh` must fit in the install script argument (about 128 KiB
  minus `lib.sh`).

## High-level design

- **Identity** (`packs/id.rs`): `PackId(Arc<str>)` is the config key.
  `PackId::parse` returns `PackRef`:
  - `Builtin{name}` (`claude`)
  - `Local{name}` (`~foo`)
  - `Project{path}` (`./packs/foo`, normalized, no `..`, no empty or
    `.` segments)
  - later `Github{owner, repo, name}`

  `installs.json` uses the full id. Mount dir names under
  `data/share/all/packs/`: `local.<name>` and `project.<sha256(path)[..16]>`.
  `.` is not valid in an identifier, so these cannot clash with built-in
  names, and they contain no `~` or `/`.
- **Source**: `PackSource::{Builtin, Local{dir}, Project{dir}}` in pack
  metadata.
- **Owned data**: `Arc<str>` for setup and Lua config,
  `version: Option<String>` (`None` for non-built-ins),
  `content_hash: Option<[u8;32]>`.
- **Loader split**: `packs/loader.rs` holds the shared validation and
  takes a file map. `builtin.rs` reads from `include_dir`. The new
  `packs/disk.rs` reads one pack dir from disk, for `~` and `./` packs.
- **Disk reader** (`packs/disk.rs`):
  - Read children with `read_dir` → `entry.file_name()` → `join`
    (`entry.path()` is disallowed). `symlink_metadata` must show a
    regular file (no symlink).
  - Read files with `read_to_string_capped`.
  - Refuse symlinks and subdirs. Skip hidden and backup files.
  - Caps: 256 KiB per file, 1 MiB per pack.
  - Content hash: sha256 over the sorted (name, len, bytes) of the used
    files.
- **User pack dirs**: each settings dir goes through `expand_tilde`,
  then an absolute check, then `VerifiedPath::new_follow` (a user-picked
  entry point, like `data_dir`).
  - Pack directory names must be identifiers. Max 64 packs per dir.
  - Settings `dirs: Option<Vec<String>>`. `None` means the default dir,
    which is silent when missing. `Some` gives a warning for a missing
    dir. Check the `smart_config` support for `Vec<String>` and
    `nest` first.
- **Project packs**: loaded on demand while the config resolves. The
  path is relative to the project root. This is the `host_cwd` project
  dir that `config::load` uses (`cmd_start.rs:94`), not `context.work`.
  It goes through `join` segment by segment, so a symlink or `..` is
  refused and the path stays inside the project.
  - New `Layer.kind: LayerKind { UserFile, Local, ProjectRoot,
    Generated }` with `#[serde(default)]`, so stored last-run configs
    still load. The pattern is the same as `Layer::from_user_file`
    (`config/mod.rs:84`).
  - `./` entries are allowed only in `ProjectRoot` layers
    (`airlock.toml`, `airlock.local.toml`). They are not allowed in
    `Local` (`.airlock/` or sandbox dir), `UserFile` or `Generated`.
  - The wizard does not list them. Template builds
    (`templates/create.rs:105`, empty build project) refuse them with a
    clear error.
- **Manager**: `init(settings, home)` loads built-ins and `~` packs.
  `offered()` (newest built-ins + `~` packs) replaces `builtin()`.
  `resolve(&PackRef, version, project_root)` loads `./` packs through
  the disk reader.
- **Two-step resolve** (`config/mod.rs:194`):
  - `resolve` takes a `ResolveInput { data_dir, packs, overrides,
    project_root: Option<&VerifiedPath>, approval: ApprovalMode }`.
    `CheckInput` (`config_change.rs:42`) and `cmd_info::resolve_config`
    pass the project root too.
  - `select()` reads the layers, and `configure_packs` loads the pack
    files. No Lua runs.
  - `approve()` checks each `~` and `./` pack against the approval
    store.
  - `evaluate()` runs `pack_configs` (the Lua) and `merge_config`.
  - Modes:
    - `Prompt`: the first resolve in `cmd_start` (`cmd_start.rs:114`).
      The terminal check (`can_prompt`, `yes`) moves before that
      resolve. With `--yes` or no terminal, the mode is `Refuse`.
    - `Refuse`: `info`, `config` (also `--resolved` of the stored
      config), `config_change::resolve_last` (it runs under the lock),
      template builds, and the wizard `check_answers`.
      `PacksNeedApproval` in `resolve_last` goes to the existing
      "last config invalid" branch (`config_change.rs:126`).
  - With `Refuse`, an unapproved pack gives a typed error
    `PacksNeedApproval(Vec<PackId>)`. The message says to run
    `airlock start`.
  - **Enforced in types**: `ConfiguredPack::config_values` (the only
    Lua entry point, used by `pack_configs` and by the wizard form at
    `form.rs:590`) takes an `Approved<'_>` token. Only the approval
    module can make one, so no code path can skip the check. Test
    helpers get a `#[cfg(test)]` constructor.
  - **No TOCTOU**: the hash is computed from the same in-memory file
    text that `evaluate` runs. Files are never read again after the
    approval. A test changes the file after approval and checks that the
    old approved text runs, or that the run fails.
- **Approval store** (`packs/approval.rs`): `pack-approvals.json` in the
  data dir. `DataDir`/`VerifiedPath`, 0600, read cap at the call site.
  - Key: `~` packs use the resolved pack dir (`new_follow`). `./` packs
    use the resolved project root plus the normalized relative path, so
    the same `./packs/foo` in two projects has two approvals. A rename
    gives a new key and a new prompt.
  - Value: content hash, approval time, and a file snapshot (the same
    format as the install snapshots) so airlock can show a diff.
  - Prompt: a new pack shows all its files, a changed pack shows the
    unified diff against the snapshot. Choices: Approve / Abort.
  - The wizard records approval when the user selects a pack, before
    any form-time Lua (`form.rs:590`) or the `check_answers` resolve.
    `templates create` relies on this: its build resolve uses `Refuse`.
  - A sandbox seeded from a template resolves its project config in the
    normal `start` flow (`Prompt`).
- **Setup env**: new Lua global `setup.env`. `setup.sh` gets it as
  `AIRLOCK_PACK_ENV_<KEY>`.
  - Keys must match `[A-Z][A-Z0-9_]*`. Values are strings with no NUL.
  - Size limits: one `K=v` ≤ 128 KiB (MAX_ARG_STRLEN), total ≤ 1 MiB
    (check the RPC frame limit first), max 64 keys.
  - Error if a pack sets `setup.env` and has no `setup.sh`.
  - `SetupEnv` newtype: redacted `Debug`, no `Serialize`.
    `InstallerScript` and the `ProcessSpec` tracing hide the values.
  - The config-change diff shows only key names.
- **Fingerprint**:
  - Built-in with no setup env: byte-identical to today. A golden test
    records the current values before the refactor.
  - Built-in with setup env: adds `env_mac`.
  - `~` and `./` packs: `(id, args, content_hash, env_mac?)`.
  - `env_mac` is HMAC-SHA256 of the sorted env pairs. Key and value are
    both length-prefixed. The HMAC key is random, in `<data>/pack-env.key`
    (0600).
- **Install snapshots**: `pack-snapshots.json` in the sandbox dir (0600,
  read cap 4 MiB), keyed by fingerprint.
  - An entry holds the pack id, args, per-file sha256 + text (≤ 64 KiB,
    else hash only), and a per-key env HMAC. Built-ins store args and env
    keys only.
  - Templates copy the file. Airlock prunes it to the live records.
- **Changed pack on the disk** (after approval):
  - `plan.rs`: add `Why::Changed` (reinstall on top) and
    `Transition::Accept{id, fingerprint}` (keep old). `Why::Changed`
    applies to records with status `installed`, `unconfirmed` or `kept`.
    `Accept` keeps the status.
  - The question shows the diff against the install snapshot and "Setup
    values changed: A, B". If the approval prompt already showed the same
    diff in this run, only the choices show.
  - Choices: Re-create (default, `--yes`) / Reinstall on top / Keep old /
    Abort. With no terminal and no `--yes`, exit 2.
  - Seeded sandboxes ask too. Both `tool_changes` calls get the `seeded`
    flag, the early one (`sandbox.rs:305`) and the late one. On the
    exit-2 path, `SeedGuard` (`sandbox.rs:279`) removes the seed.
    Re-create on a seeded disk calls `templates::seed::unseed` and resets
    `InstallState`.
  - Stale disk (`Transition::Reset`): all packs install fresh, so
    airlock does not ask the question.
  - `templates create` from a base template (`templates/create.rs:174`)
    uses the same helper.

## Phases

Gate for every phase: `mise run test`, `mise run bats` (where relevant),
clippy, `mise format`. A critic review (different tier) and a WIP commit
follow each phase.

Order: P0 and P7 run in parallel with the rest. P1 → … → P6 run in
sequence, because they change the same files (`packs/mod.rs`,
`config/mod.rs`, `start/sandbox.rs`). All user texts go in
`messages.rs`. The pack-install exit codes 10–13 map to `messages` texts.

**Pre-P1: golden fingerprint test.** Record the fingerprints of the
built-in packs (default args and some changed args) with the current
code.

**P0: lib.sh API** (independent, sonnet)
- Rename the stable helpers to `airlock_*` in `packs/install/lib.sh`.
  Mark the rest private.
- Update `packs/*@1/setup.sh`, `install/compose.rs`, and
  `packs/tests/test_setup_script.rs`.

**P1: identity, owned data, `other` kind** (opus)
- `packs/id.rs`, `packs/loader.rs`, `builtin.rs`, `mod.rs`.
- `lua_config.rs`: `pack.version` is nil for non-built-ins.
- Wizard `form.rs` `KINDS`, and the `view.rs` section title (messages).
- `config/generated.rs` and `templates/mod.rs`: version becomes
  `Option`. `TemplateConfig::parse` (`templates/mod.rs:98`) accepts no
  version only for non-built-in ids.
- All callers of `builtin()`, `init()` and `resolve(name, version)`:
  `config/mod.rs:200`, `pack_entries.rs` (`known`, `supported_versions`,
  `unknown_pack`), `sandbox.rs:554` `label()`, and the test fixtures
  (`init_with_sample`, `load_test_packs`, `builtin::fixture`,
  `legacy_presets.rs`).
- No behaviour change except optional `kind`.

**P2: user packs** (opus)
- `settings/mod.rs`: `PackSettings { dirs, defaults }`.
- `packs/disk.rs` plus the dir scan.
- Update the `init` callers: `cmd_start`, `cmd_info`, `cmd_config`,
  `templates/create`.
- `pack_entries.rs`: `~` rules and errors.
- Wizard: `offered()`, `(local)`, pre-select `defaults` (warn on unknown
  names). `generated.rs` writes quoted keys with no version.
- Tests `packs/tests/test_user_packs.rs`: real temp dir, symlink,
  duplicate, caps, missing pack in `config --resolved`.

**P3: project packs** (opus)
- `PackRef::Project`, path parse and normalize, the project root in
  `resolve`, `LayerKind` and the layer restriction. Template builds
  refuse `./` packs.
- Tests `packs/tests/test_project_packs.rs`: `./` path resolves, `..`
  and symlink are refused, the pack is refused in a user file and in
  `.airlock/airlock.toml`, missing pack.

**P4: approval gate** (opus)
- Split `resolve` into select, approve and evaluate, with `ResolveInput`.
  Add the approval modes to all callers: `cmd_start` (move the terminal
  check earlier), `cmd_info`, `cmd_config`, `config_change::resolve_last`,
  `wizard::check_answers`, the wizard form, and templates.
- `Approved` token on `config_values`. Update the test callers
  (`config/tests/test_builtin_packs.rs:28`,
  `packs/tests/test_configure.rs:45`).
- `packs/approval.rs` store, the shared snapshot format
  (`packs/snapshot.rs`), and the prompt with diff. Reuse the diff
  helpers of `config_change.rs`.
- Tests `config/tests/test_pack_approval.rs`: Lua does not run before
  approval (the test Lua writes a marker file), a changed hash asks
  again, `Refuse` errors, a wizard selection approves.

**P5: setup env + fingerprint** (opus)
- Before you start: check the vsock RPC frame limit for `ProcessSpec`.
  If there is no limit, use 1 MiB and add a bats test with a 1 MiB env.
- `lua_config.rs` `setup` table and limits. `SetupEnv` in
  `ConfiguredPack`.
- `setup_installer` env and fingerprint, plus the HMAC key helper.
  Redaction in `InstallerScript`, `ProcessSpec` and the spawn tracing
  (`install/setup.rs:326`).
- Key list in `config_change::final_values`.
- Tests: the golden fingerprint, limits, no value in debug output or the
  diff.

**P6: disk changes + snapshots** (opus)
- `plan.rs`, `state.rs`, `sandbox.rs` (`ToolAnswer::{Reinstall, Accept}`,
  `seeded`), `messages::start`.
- `pack-snapshots.json`, written from `start/install.rs` on state save.
  Add it to `templates::FILES`, `seed.rs` (copy + unseed) and
  `create.rs` `keep_files`. The file can be missing: update
  `test_template_sandbox.rs:53`. Check all users of the state files
  (`sandboxes/migrate.rs`, `sandboxes/mod.rs:254`, `STATE_FILE`).
- E2E tests `start/tests/test_changed_disk_pack.rs`: an existing sandbox
  and a template seed, all three choices.

**P7: deprecation warning** (independent, sonnet)
- `LayeredConfig::has_user_files()`, then a warning in `cmd_start`
  (`messages::start::USER_CONFIG_DEPRECATED`).

**P8: docs** (last, sonnet + STE linter)
- `packs.md` (user and project packs, approval, `other`).
- `technical/packs.md` (layout, `airlock_*` API, exit codes 10–13,
  `setup.env`, limits, fingerprint, approvals, snapshots).
- `configuration.md` settings table, `templates.md`.
- `docs/log/2026-10-28-user-and-project-packs.md`.

## Open points

1. "Continue with last config" resolves the stored layers with the
   current pack files, after approval. Document this.
2. `installs.log` captures script output, so a pack that echoes a setup
   value leaks it. Document "do not print setup values".

## Verification

- Unit and E2E tests per phase (above).
- Manual check in `.tmp/test-packs`, with a temp `HOME`, a
  `~/.airlock/packs/hello/` pack and a project `./packs/setup/` pack.
  Each pack has `config.lua` (with `setup.env`) and `setup.sh`.
  1. `mise airlock -- start`: the wizard shows `Hello (local)`, selects
     it, writes `"~hello" = {}`, and the install runs with no approval
     prompt.
  2. Add `"./packs/setup" = {}` to `airlock.toml`, then start: the
     approval prompt shows the files before any Lua runs.
  3. Edit `config.lua` of each pack: the approval prompt shows the diff.
     `airlock info` gives the approval error.
  4. Edit `setup.sh`, then start: the disk prompt shows the three
     choices. Check each choice.
  5. Create a template with `~hello`, edit the pack, start a new sandbox
     from the template: the disk prompt shows.
  6. Delete the pack: start and `config --resolved` fail with a clear
     error.
  7. Add `~/.airlock/config.toml`: the deprecation warning shows.
