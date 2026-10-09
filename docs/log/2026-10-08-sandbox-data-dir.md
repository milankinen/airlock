# Keep sandbox data out of the project directory

Each project kept its sandbox in `<project>/.airlock/sandbox/`: the CA
private key, the disk image, the install records and the sockets. The
project directory is shared to the guest. Only a guest-side bind mount
hid `.airlock`, so a guest could reach the data through another mount of
a parent directory, or after it escaped the mask. New sandboxes now live
in the airlock data directory.

Plan: `docs/plans/2026-10-08-sandbox-data-dir.md`.

## Data directory

- One root for all airlock data. The default is `dirs::data_dir()/airlock`:
  `~/Library/Application Support/airlock` on macOS,
  `$XDG_DATA_HOME/airlock` (`~/.local/share/airlock`) on Linux. The
  `data_dir` setting changes it.
- The root holds the database (`db/`, it was `~/.airlock/db`), the
  sandboxes (`boxes/<id>/`) and all of the former `~/.cache/airlock`
  (`oci/`, `vm/`, `packs/mounts/`, `sock/`). `~/.airlock/db` was not
  released, so it has no migration. `~/.cache/airlock` was released, see
  `2026-10-09-legacy-cache-move.md`.
- One root keeps the `image` hard link of a sandbox and the image cache on
  one file system.
- `~/.airlock` keeps only the settings and the vault.

## Registry and sandbox records

- The global database has a new `sandboxes` database: `id → canonical
  project path`. It has only the sandboxes in the data directory. The id
  is 8 random base32 characters. A collision makes a new id.
- Both kinds of sandbox keep the same layout as before: `ca.json`,
  `run.json`, `installs.json`, disk, image link, lock, sockets and logs.
  A first version kept these records in a per-sandbox LMDB store. It was
  removed again: the location gives the security gain, and atomic file
  writes under the sandbox `flock` already let readers read without a
  wait. The store cost about 500 lines (import of old files, symlink
  checks for LMDB files in the project tree, LMDB handle pitfalls).

## Location and move

- `sandbox_location = "cache-dir" | "project-dir"` (default `cache-dir`)
  selects where a new sandbox goes.
- `airlock start` finds a sandbox with one lookup for both kinds
  (`sandboxes::resolve_sandbox`). For a project sandbox with `cache-dir`,
  it asks to move the sandbox (default yes, `--yes` = yes). "No" writes
  an empty `keep-in-project` marker, so start does not ask again. Without a terminal, the
  sandbox stays and start asks on a later run.
- The move is one `rename` under the old sandbox lock. A cross-device
  rename is refused with a message. The move registers the id before the
  rename and removes the entry if the rename fails.
- A new record is made only after the `[env]` check. Thus a failed start
  leaves no empty sandbox.
- The log goes to `<sandbox>/airlock.log`. Lines from before the sandbox
  is known wait in memory. The log file opens through `PinnedDir`
  (no symlink is followed).

## Local project config and `.airlock`

- The local project config (`airlock.<ext>`, written by the `start`
  option of the setup wizard) of a sandbox in the data directory is in the
  sandbox directory, never in the repository. A project sandbox keeps it
  in `.airlock/`. Config loading, the wizard save, `show` and `rm` use
  the same rule.
- The move also moves the local config and then removes the whole
  `.airlock/`. Known entries are `.gitignore`, `airlock.log`, `sandbox`
  and the local config files. Any other entry stops the move before
  anything changes: ".airlock directory contains extra files (...), can't
  migrate - remove extra files or continue using current directory".
- A new sandbox in the data directory takes a local config out of
  `.airlock/` in the same way.
- The move question is a list: "Yes, migrate" (default), "No, keep
  current", "Cancel".
- The guest masks `<project>/.airlock` only for a project sandbox. The
  `boot` RPC has a new `skipAirlockMask` flag. Its default (false) keeps
  the mask, so a host that does not set it fails closed. Without the
  flag, the mask made an empty `.airlock` in the host project on each
  boot of a data-directory sandbox.

## Commands

- `airlock exec` looks in the cwd and its parents. For each directory, it
  checks the registry and then `.airlock/sandbox`. The first sandbox with
  a `cli.sock` wins.
- `airlock rm` removes the sandbox of the cwd (no parent lookup, as
  before). For a sandbox in the data directory, the local config in
  `.airlock/` stays.
- New `airlock sandbox list|ls`, `info [ID] [--json]` and
  `remove|rm [ID]... [-f]`. The list has only registered sandboxes.
- `airlock show` is now `airlock info` (`show` stays as an alias). It and
  `airlock rm` are shortcuts for `airlock sandbox info` and `airlock
  sandbox rm` without an id: same code, same output. For a sandbox whose
  project directory is gone, `info` shows only the sandbox part.

## Not done

- No VM run in this environment (no KVM). The VM bats (`tests/vm`) and
  the KVM-only cli bats are not verified. `tests/vm/packs.bats` finds
  the sandbox directory through `airlock sandbox info --json`.
