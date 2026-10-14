# Remove project-owned sandboxes

## Context

The security audit of `io-hardening` found host escapes that exist only when the
sandbox dir is `<project>/.airlock/sandbox`. The guest can write that dir:
- vsock symlink: the guest gets any host Unix socket
- the hard links of read-only file mounts: the guest can write those host files
- the `image` hard link: the guest can change the shared image cache

The fix: all sandboxes live in `<data>/boxes/<id>`. An old project sandbox gets a
hardened import: the import does not trust its content.

User decisions:
- Remove the `security.sandbox_type` setting and the `keep-in-project` choice, with
  their code and docs.
- The start finds an old `.airlock/sandbox` (a `keep-in-project` marker does not
  change this) and shows one short message, with the choices Continue / Abort.
  Message: `Sandbox data moves to the airlock data dir for security.`
- No TTY and no `--yes`: stop with an error and a hint. `--yes`: move.
- Hardened move:
  - Take only the known entries, as regular files with one link, opened with
    `O_NOFOLLOW`.
  - Drop `run.json` and the `image` link.
  - Remove the old dir as an entry after the import succeeds.
- Config change check: a sandbox with a disk and no stored config gets the
  question (fail closed).

## High-level design

- **One sandbox location.** `Location::ProjectDir`, `KEEP_IN_PROJECT`,
  `SandboxLock::acquire(host_cwd)`, `Project::sandbox_in_project` and the
  `.airlock/sandbox` probe in `candidates()` go.
  - `Found.dir` is always `boxes/<id>`.
  - `local_config_dir` always returns the sandbox dir.
  - `find_local_config_dir` keeps the `<project>/.airlock` fallback for a project
    with no sandbox yet. `ensure_cache_dir` stays for the wizard local config.
  - `PROJECT_SANDBOX` stays only for the legacy detection.
- **Guest mask goes.** Remove `skipAirlockMask`. It is the last boot param, and
  airlockd ships in the embedded initramfs, so both sides change together. Also
  remove the host setter and `mask_airlock_dir` in airlockd. User `masks` stay.
- **Legacy import** (`sandboxes/migrate.rs`, `import_legacy`). The import trusts no
  content from the old dir. A cloned repo can ship `.airlock/sandbox`.
  1. Refuse a symlink or non-dir at `.airlock` or `.airlock/sandbox`. Run
     `check_known_entries(.airlock)`. Pin the old dir (`PinnedDir::pin`, then an
     owner check on the fd). Take `lock` through the pinned fd (`O_NOFOLLOW`,
     `flock` NB). A held lock gives `Running`.
  2. Note whether the registry already has the project. Then
     `find_or_register` and `lock_box`.
  3. Take the allowlist. Each source opens relative to the pinned fd with
     `O_NOFOLLOW|O_NONBLOCK`. `fstat` must show a regular file, `nlink == 1` and
     our uid. Missing or refused entries are skipped.
     - `disk.id`: capped read, 32 hex chars.
     - `disk.img`: only with a valid `disk.id`.
       - Use `renameat(old_fd, new_fd)`, then check that the new entry has the
         same dev/ino and `nlink == 1`.
       - On `EXDEV`, use a reflink or sparse copy from the opened fd.
       - Then `fchmod 0600`.
       - Print one progress line for a copy.
     - `installs.json`: only with the disk. Must parse. Clean it: set
       `ran_session = true`, keep only the `installed`/`kept` records, and make
       the others `failed`. Then the "session ran on this disk" question stays
       on.
     - `ca.json`: only with the disk. Pack installs on the disk use it. Must
       parse. A planted CA key harms only that guest.
     - `image`: do not copy it. Take `image_id` (capped read, must be
       `sha256:<64 hex>`). If `<data>/oci/images/<digest>` exists, link it with
       `ensure_image_hardlink`. Else write no image file and mark the import,
       so the next prepare treats it as an image change (question).
     - All other entries are not taken: `run.json`, `lock`, logs, sockets,
       `overlay/`, `vfs/`, `keep-in-project`, temp files, unknown entries.
  4. Move the local config (`move_local_config`, which also removes `.airlock`).
     After a successful import, a failure to remove is a warning.
  5. On an error before step 4: remove the new box. Unregister only if step 2
     made the id. A file that `renameat` moved goes back. The old dir stays.
  - The `CrossDevice` error, `RUNTIME_ENTRIES` and `remove_vsock_listeners` go.
  - The `ExtraFiles` message gets a hint to move or remove the listed names.
- **Start flow.** `lock_sandbox` (`start/location.rs`):
  - The registry has the project: `lock_box`. A stale `.airlock/sandbox` is
    ignored.
  - Else, if `<project>/.airlock/sandbox` exists (`symlink_metadata`), run
    `ask_import`:
    - `--yes`: Continue.
    - No TTY: exit 1 with `LEGACY_SANDBOX_NO_PROMPT`.
    - Else Continue (default) / Abort.
  - Else: register a new box.
  - `cmd_start.rs:100` `has_content` also counts a legacy dir, so an old
    project does not get the setup wizard before the import.
- **Config change check** (`check_with`). If `last_run_config` is `None` and
  `has_disk`:
  - Use the "last config invalid" path: diff `""` → current, with the note
    `LAST_CONFIG_MISSING`, and the choices Abort / Accept.
  - No TTY or `--yes` without `--allow-configuration-changes` gives exit 2.
  - Invariant (tested): a `--yes` import of a sandbox with a disk exits 2.
  - A data-dir sandbox from a version with no config copy gets this question
    one time. Write this in the docs and the log.

## Detailed plan

Each phase ends with `mise format`, `mise run lint` and `mise run test`, then a critic
review (a different agent) and a WIP commit.

### Phase 1: import and fail-closed config check

1. `util/safe_fs.rs`:
   - Add an `nlink == 1` check to `PinnedDir::read`, or add a flag. Check the
     users that read files the guest could link.
   - Add `PinnedDir::open_regular(name) -> io::Result<Option<File>>`: `O_RDONLY`,
     the checks above, `None` for `ENOENT`.
   - Add `rename_into(name, &PinnedDir, name)` (`renameat`).
   - Add an owner check after `pin`. Add a way to `flock` a file in the pinned
     dir.
2. `util/clone.rs`:
   - Add `clone_from_file(src: &File, dst_dir: &PinnedDir, name)`. Use FICLONE
     on Linux, `fclonefileat` on macOS, then the sparse copy. Then `fchmod 0600`.
   - `clone_file` uses it, or stays the same if that is simpler.
3. `sandboxes/migrate.rs`: `import_legacy` as in the design.
   - The new tests use `test_cfg/sandboxes.rs` `legacy_sandbox`. Add a planted
     symlink, a FIFO, a hard link, a bad `disk.id`, an `installs.json` with
     `ran_session = false`, an `image` with a cached and an uncached digest.
4. `start/config_change.rs`: add the fail-closed branch and update the module doc.
   `messages.rs`: add `LAST_CONFIG_MISSING`.
5. Rust tests in this phase:
   - Rename `sandboxes/tests/test_migrate_links.rs` to `test_import_legacy.rs` and
     extend it with the cases above. Also check: the targets stay untouched,
     the disk stays sparse with the same content, the rollback leaves no box
     and no registry entry, and the old dir is gone.
   - Update `start/tests/test_sandbox_location.rs`: the move tests now expect a
     dropped `run.json` and a re-linked or absent `image`.
   - `test_config_change.rs`:
     - Change `sandbox_without_config_copy_starts_and_gets_copy`. A disk and no
       copy with no terminal gives exit 2. `--allow-configuration-changes`
       passes.
     - Add a test: a new sandbox with no disk passes with no question.

### Phase 2: remove project-owned code paths

1. `settings/mod.rs`:
   - Remove `sandbox_type` and `SandboxType`, and their tests.
   - Rework the bad-value test with another key.
   - Add a test: an old `settings.toml` with `security.sandbox_type` still loads.
2. `sandboxes/mod.rs`: remove `KEEP_IN_PROJECT`, `Location::ProjectDir` and the
   `candidates()` probe. Collapse `Location` if this stays simple.
   `local_config_dir` always returns the sandbox dir.
3. `start/location.rs`: the new `lock_sandbox` and `ask_import`. `messages::start`:
   - Remove `MOVE_*` and `sandbox_data_visible`.
   - Add `IMPORT_TITLE` ("Sandbox data moves to the airlock data dir for
     security."), `IMPORT_CONTINUE` ("Continue") and
     `LEGACY_SANDBOX_NO_PROMPT` ("Old sandbox in .airlock/sandbox. Run
     `airlock start` in a terminal, or with --yes, to move it.").
   - Reuse `cli::ABORT_CHOICE`.
4. `cli/cmd_start.rs:100`: `has_content` counts a legacy dir.
5. `project/mod.rs`: remove `SandboxLock::acquire` and `sandbox_in_project`. Fix
   the comments about a guest-writable sandbox (keep the caps).
6. `cli/cmd_rm.rs`: remove the project branches and helpers (`rm_project_sandbox`,
   `rm_sandbox_only`, `rm_symlinked_airlock`, `is_user_home_project`,
   `passwd_home`, `user_file_marker`, `remove_entry`) and their messages.
7. `cmd_info.rs`, `cmd_exec.rs`, `cmd_sandbox.rs` and `cmd_config.rs:67`: drop the
   project branches. The JSON `location` stays, always `"data-dir"`.
8. Guest mask: remove `skipAirlockMask` (capnp), `rpc/supervisor.rs:225`,
   `airlockd` `rpc.rs:299`, `init/mod.rs:94` and `init/linux/overlay.rs:206-215`.
9. Tests:
   - `test_cfg/start.rs:271`: change the probe to the data-dir path.
   - Rewrite or delete the project tests: `test_rm.rs`, `test_exec_lookup.rs`,
     the `project/mod.rs` lock tests, the `cmd_info.rs` unit test,
     `test_image_cache.rs:73`, `test_config_layering.rs:180` and
     `test_sandbox_location.rs` (the project-owned setting and kept cases).
   - Add start tests:
     - An old dir with `keep-in-project` and no terminal: error, nothing
       changes.
     - `--yes`: import, then exit 2 at the config check.

### Phase 3: bats tests

- `tests/helpers.bash:164-191`:
  - `make_sandbox_disk` and `write_installs` write into a registered box. Make
    it with a start that has `--allow-configuration-changes`, or register the
    box directly.
  - Also write a `last_run_config` where tests need a start with no question.
  - Keep a `make_legacy_sandbox` for the import tests.
- `start.bats`:
  - Replace the two move tests: `--yes` import (exit 2 without
    `--allow-configuration-changes`, files taken), and no terminal gives an
    error.
  - Fix the path asserts.
- `sandboxes.bats`, `info.bats`, `rm.bats`, `config.bats` and
  `config_loading.bats`: data-dir fixtures.
- `tests/vm/mounts.bats:97-124`: delete the project mask tests.
- Run `mise run bats`. If VM tests cannot run here, report that.

### Phase 4: docs

Use the `/asd-ste100` skill (STE mode). Lint with
`mise x -- python .claude/skills/asd-ste100/scripts/ste-lint.py <pages>`.
- `configuration.md:36`: remove the row.
- `usage/managing-sandbox.md`:
  - Replace `:159-178` with a short "Sandboxes of older versions" note about the
    import question and the one-time config question.
  - Fix `:127-129`.
- `technical/project-layout.md:68-137`: one location, the import rules.
- `technical/container-execution.md:96`: change the lookup steps.
- `technical/io-security.md`: sandbox data is never guest-writable, the import
  rules and the fail-closed check. Remove the `airlock rm` symlink line.
- `configuration/disk.md:3`, `tips/vibe-config.md:7`, `mise.toml:78`: wording.
- New `docs/log/2026-10-14-remove-project-sandboxes.md`:
  - The audit reason and the decisions.
  - Residual risks: a guest can plant `.airlock/sandbox`, and after `airlock rm`
    the next start offers to import it. The import rules limit the damage.

## Verification

- `mise format`, `mise run lint`, `mise run test` after each phase. `mise run bats`
  at the end.
- Manual test, from `.tmp/test-legacy`:
  1. Make an old sandbox dir with `disk.img`, `disk.id`, `ca.json`, `run.json`,
     `image`, `installs.json` (`ran_session = false`), a symlink `evil -> ~/.ssh`
     and a FIFO.
  2. Run `mise airlock -- start --yes -- true`. Expect exit 2 (config check).
  3. Check:
     - only the allowlisted files are in `<data>/boxes/<id>`, and the symlink
       and FIFO are not followed
     - `.airlock/sandbox` is gone
     - `ran_session` is true
  4. Run with no TTY and no `--yes` on a new legacy dir: expect an error and no
     change.
