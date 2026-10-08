# Host sandbox data outside the project dir

## Context

Each project's sandbox data lives in `<project>/.airlock/sandbox/` today.
The project dir is shared to the guest over virtiofs, and only a
guest-side read-only bind hides `.airlock`. So the guest can reach the CA
private key, disk image, install state and sockets. It can do this
through any other mount of a parent dir, or after it escapes the mask.
New sandboxes therefore move out of the project tree, into a per-user app
data root.

## Decisions

- **Root** (`data_dir` setting): the default is `dirs::data_dir()/airlock`.
  This is `~/Library/Application Support/airlock` on macOS and
  `$XDG_DATA_HOME/airlock` (`~/.local/share/airlock`) on Linux. It holds
  everything that `~/.cache/airlock` and `~/.airlock/db` hold now:
  - `db/`, the global database
  - `boxes/<id>/`, the cache sandboxes
  - `oci/`, `vm/`, `packs/mounts/`, `sock/`

  One root keeps the image hardlink (`boxes/<id>/image` →
  `oci/images/<digest>`) on one filesystem. Settings and vault stay in
  `~/.airlock`. There is no migration of `~/.cache/airlock` or
  `~/.airlock/db`, because they are not released. Project-dir sandboxes
  on a different filesystem than the root keep today's behavior: the
  image hardlink fails with a clear error.
- **`sandbox_location = "project-dir" | "cache-dir"`**, default
  `cache-dir`.
- **Ids**: 8 chars of lowercase base32 (`a-z2-7`, 40 bits) from `SysRng`.
  On a collision (registry key or dir exists), loop and make a new id.
- **CLI socket**: `<box>/cli.sock` as today. The path is
  `~/Library/Application Support/airlock/boxes/abcdefgh/cli.sock`, about
  70 chars. The hash fallback in `cache::cli_sock_path` stays for long
  home paths.
- **Global database** (`<root>/db`, the existing `Db`): a new named
  database `sandboxes` with `id → canonical project path` only. Lookup by
  path is a scan, which is fine for a small number of boxes. Only cache
  sandboxes are registered. Project-dir sandboxes are never in it and are
  not listed.
- **Sandbox records stay files** (revised after the first
  implementation): both kinds of sandbox use the same layout as before
  (`ca.json`, `run.json`, `installs.json`). The security gain comes from
  the location, not from the storage format. Atomic writes under the
  sandbox `flock` give readers a whole file without a wait. A per-sandbox
  LMDB store was implemented first and then removed: it added about 500
  lines (store, import of the old files, symlink checks for LMDB files in
  the project tree) for no behavior gain.
- **Keep marker**: "No" to the move writes an empty `keep-in-project`
  file in the project sandbox.
- **Resolution**: one function, `resolve_sandbox(start_dir, parents: bool)
  -> Option<Found>`. For each candidate dir (only `start_dir`, or
  `start_dir` and its parents), it checks:
  1. A registry entry whose path is the dir → `Found::Cache { id, dir }`.
  2. Else `<dir>/.airlock/sandbox` is a real dir (not a symlink) →
     `Found::Project { dir }`.

  The first hit wins. Users:
  - `exec`: `parents = true`. It also skips a hit that has no live
    socket and keeps walking, as today.
  - `rm`, `show`, `sandbox info`, `start`: `parents = false`.
- **Start**:
  - No hit → create a new sandbox of the configured kind. This happens
    after `check_env_early`, so failed starts leave nothing.
  - `Project` hit and `cache-dir` set, and the local database has no
    `keep-in-project` marker → ask "Move this sandbox out of the project
    directory?" (default yes, `--yes` = yes):
    - Yes → migrate.
    - No → write the `keep-in-project` marker and never ask again.
    - No terminal → use it as is, and ask on a later interactive start.
  - With `project-dir` set → never ask.
  - Migration (under `lock_if_idle(Held)` of the old dir, with a re-check
    of the registry):
    1. Import the legacy files.
    2. Remove the runtime files (sockets, `lock`, `overlay/files`).
    3. `rename` to `<root>/boxes/<id>`.
    4. Register the id.

    On `EXDEV`, refuse with a message that names `sandbox_location =
    "project-dir"` and `airlock rm`.
- **Log**: `airlock.log` goes into the sandbox dir.
  - Lines before the dir exists stay in memory and are flushed when it
    exists.
  - The file is opened through `PinnedDir`, so no symlink is followed.
- **Commands**:
  - `airlock sandbox list|ls`: registered boxes only. Columns: ID,
    STATUS, LAST RUN, DISK, PROJECT. A missing project path shows
    `(missing)`.
  - `airlock sandbox info [ID]`: with no ID, the sandbox of the cwd,
    either kind. `--json` gives machine output (used by the bats tests).
  - `airlock sandbox remove|rm <ID>... [-f]`: refused if running. Asks to
    confirm unless `-f`. Removes the dir and the registry entry, then
    runs `oci::gc_sweep`.
  - `airlock rm`: the cwd sandbox of either kind. For a cache box it
    removes the box and the registry entry, then keeps today's `.airlock/`
    handling (local config question, home markers, symlink cases).

## Database layout

Global database `<root>/db`, named database `sandboxes`
(`Database<Str, Str>`):

| Key | Value |
|---|---|
| `<id>` e.g. `k3x7q2ma` | canonical project path, e.g. `/Users/mla/dev/foo` |

`disk.id` stays a file next to `disk.img`. `reset_disk` removes the two
files together.

## Implementation phases

1. **Root + settings**:
   - `cache.rs` → root from settings (`data_dir`, `dirs::data_dir()`
     default). Add `db_dir()` and `boxes_dir()`.
   - `settings.rs`: the `SandboxLocation` enum (pattern of
     `VaultStorageType`), and `data_dir: Option<String>` with `~`
     expanded.
   - `context.rs` opens the global database at `<root>/db`.
   - Tests: `test_context` points the root into the temp home.
2. *(Removed: per-sandbox database. The records stay files.)*
3. **Registry + resolution** (`sandboxes.rs`): the `sandboxes` database
   (`register`, `unregister`, `list`, `find_path`), the id generator,
   `resolve_sandbox` and `migrate`.
4. **Start flow**:
   - `init_logging` with a deferred writer.
   - `ensure_sandbox` resolves or creates the sandbox and asks the
     migration question.
   - `SandboxLock::acquire(&found)` re-checks the registry under the lock.
   - Switch the hardcoded `.airlock/sandbox` sites to the resolved dir:
     `start/sandbox.rs:227`, `wizard.rs:191`, `setup.rs:348`,
     `cmd_show.rs:281`, `report.rs:22`, and the `vt100_replay` doc.
5. **exec / rm / show** on `resolve_sandbox`. `main.rs` passes `Context`
   to `exec` and `rm`.
6. **`airlock sandbox` command** (`cli/cmd_sandbox.rs`, the pattern of
   `cmd_secret.rs`).
7. **Tests**:
   - Rust end-to-end in `sandboxes/tests/`:
     - `test_new_sandbox_in_data_dir`
     - `test_legacy_migration` (yes / no / project-dir setting / no
       terminal)
     - `test_exec_resolution` (cache and project mixed in one ancestor
       chain)
     - `test_sandbox_remove`
   - Update the existing tests: `test_rm.rs`, the `project.rs` tests,
     `test_image_cache.rs`, `test_config_layering.rs:169`,
     `start/tests/test_sandbox.rs`, `packs/tests/test_install.rs`,
     `test_cfg/{start,packs,network}.rs`. `StartProject` shares one
     `Context`.
   - Bats:
     - new `tests/cli/sandbox.bats`
     - `show`/`rm`/`config_loading`: `mkdir .airlock/sandbox` stays a
       valid project-dir sandbox
     - `vm/packs.bats` reads the log and pack status through
       `sandbox info --json`. The sed edit of `installs.json` moves to a
       Rust test.
8. **Docs**:
   - Manual pages (STE + lint): `usage/managing-sandbox.md`,
     `technical/project-layout.md`, `configuration.md`,
     `technical/{rpc,container-execution,networking,mounts}.md`,
     `usage/attaching-to-running-sandbox.md`, `usage.md`, and a settings
     section.
   - `docs/log/2026-10-08-sandbox-data-dir.md`.
   - Copy of this plan to `docs/plans/2026-10-08-sandbox-data-dir.md`.

## Verification

- `mise run test`, `mise run lint`, `mise run bats`, `mise format`.
- Manual runs in `.tmp/test-*` with `mise airlock`:
  - A new project gets no `.airlock/sandbox`. `sandbox ls` shows it, and
    `exec` from a subdir works.
  - A legacy sandbox from a `main` build: the migration yes/no flow, and
    the next start does not ask.
  - `sandbox_location = "project-dir"`: the sandbox stays in the project,
    and `sandbox ls` does not list it.

## Concurrency notes

- Readers never wait for a writer. The record files are written
  atomically (temp file and rename). Global database writes are short
  single-value saves.
- The run exclusivity is the `flock` on `<sandbox>/lock`. Other commands
  probe it non-blocking (`lock_if_idle`), so `rm` and `sandbox rm` fail
  fast and `sandbox ls` only shows the status.
- Read-only commands (`ls`, `info`, `show`) never create files.
