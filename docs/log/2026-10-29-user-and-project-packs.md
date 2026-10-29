# User and project packs

All packs came from the binary. Users had only user-level config files
for personal setup, and these files cannot enable packs. Projects had
no way to keep their setup as a pack in the repository. Plan:
[2026-10-28-user-and-project-packs.md](../plans/2026-10-28-user-and-project-packs.md).

## Change

- User packs: `"~name"` in `[packs]`, from the settings `packs.dirs`
  (default `~/.airlock/packs`). The wizard shows them with `(local)`.
- Project packs: `"./path"` in `[packs]`, relative to the project root.
  Not in the wizard, not in templates.
- No `version` for user packs and project packs. `kind` is optional,
  with the new kind `other` (also for built-ins, and a wizard section).
- Settings `packs.defaults`: packs that the wizard selects at the start.
- Approval of a new or changed `config.lua` before it runs. The
  question has `s` to show the changes. `--auto-approve-packs` approves
  with no question. `--yes` and no terminal refuse. `info` and
  `config --resolved` refuse.
- Changed packs on the disk: `Re-create sandbox` (default, `--yes`),
  `Reinstall on top`, `Keep old`, `Abort`, with `s` to show the diff.
  Also for a new sandbox from a template, and for
  `templates create` from a base template.
- `setup.env` in `config.lua` gives values to `setup.sh` as
  `AIRLOCK_PACK_ENV_<KEY>`.
- Stable `lib.sh` API: `airlock_*` functions, `DISTRO`, `ARCH`, `LIBC`,
  `AIRLOCK_PACK_TMPDIR`, exit codes 10-13. The built-in packs use the new
  names.
- Warning on `airlock start` when a user-level config file exists. The
  files are deprecated, user packs replace them.
- Manual: `packs.md`, new `packs/authoring.md`, `configuration.md`,
  `templates.md`, `technical/packs.md`, `technical/project-layout.md`.
  The obsolete `tips/vibe-config.md` page is removed.

## Design

- **Identity.** The `[packs]` key is the pack id: `name`, `~name` or
  `./path`. A project path is in the normal form, with no `..`, `.`,
  empty or hidden names. Mount directories are `local.<name>` and
  `project.<hash>`. A `.` is not valid in a name, so they cannot clash
  with built-in names.
- **Shared loader.** One loader checks the files of all sources. The
  disk reader gives it only the known files (`pack.toml`,
  `config.<ext>`, `setup.sh`). It does not list the directory. The guest
  can write the project, and a listing would let it make airlock read
  any number of files.
- **Approval before Lua.** The resolve has three steps: select (load
  files, no Lua), approve, evaluate. The Lua entry point needs an
  approval token that only the approval module makes. The hash is of
  the text in memory that runs, so a file change after the approval has
  no effect.
- **`config.lua` hash only.** Only `config.lua` runs on the host. A
  static config sets values, and `setup.sh` runs in the sandbox. Thus
  the approval covers only `config.lua`, and packs without it need no
  approval. A changed `setup.sh` gives the disk question, not the
  approval question.
- **Approval key.** The key is the source kind and the resolved pack
  directory. The same `./path` in two projects has two approvals. The
  store keeps a copy of the approved script for the diff.
- **Wizard selection approves.** The user picks the pack in the wizard.
  A second question for the same pack adds nothing. Template builds use
  the refuse mode and rely on this.
- **Fingerprint compatibility.** A built-in pack with no setup values
  keeps the old fingerprint input (id, version, non-default args). Thus
  no existing sandbox sees a change after the update. A test checks the
  fingerprint against the old input. Packs from the disk add the
  content hash.
- **Setup env secrecy.** Values can be tokens. They go only into the env
  of the install exec. Debug output, tracing, records, snapshots and the
  config diff have only keys. Changes show through an HMAC with a
  random key in `pack-env.key`, so a stored hash cannot be tested
  against guessed values without the key.
- **Snapshots.** `pack-snapshots.json` keeps the installed pack files by
  fingerprint, so the disk question can show a diff. Templates copy it.
  `Keep old` gives the old copy to the new fingerprint.
- **Template seed bug.** A sandbox from a template counted as a new
  disk. A pack that changed after the template build then stayed old
  with no question. Now the copy gets the changed-packs question, and
  `Re-create sandbox` drops the copied disk.

## Limits

- 256 KiB per pack file. `setup.sh` must fit in one exec argument with
  `lib.sh` (about 110 KiB).
- 64 packs per user pack directory, 32 project packs per config, 256
  bytes per project pack id.
- Setup env: 64 keys, 128 KiB per variable, 1 MiB per pack.
- 128 approval records. The oldest go first.

## Known limits

- A setup script that prints a setup value writes it to `installs.log`.
- "Continue with last config" resolves the stored layers with the
  current pack files. A pack that needs approval there makes the last
  config not valid.
- A template from an older airlock has no snapshots. The disk question
  cannot show the changes of its packs.
