# Refuse mounts that expose the home directory

Review finding #2 (`FINDINGS.md` in faa033a).

## Problem

v2026.9.6 always masked `<project>/.airlock` in the guest. The data
directory change skips this mask for a sandbox in the data directory.
For a project at `$HOME`, `<project>/.airlock` is `~/.airlock`. The guest
could then read the vault file (plaintext with `storage = "file"`) and
`settings.toml`, and plant `~/.airlock/airlock.toml` or `config.toml`,
which apply to all projects. The data directory, with the disks and CA
keys of all sandboxes, was also visible. A user mount of `~` or of a
parent directory had the same problem, also in v2026.9.6.

## Change

- `vm::mount::check_exposure`: before the VM starts, each directory
  mount (also the project mount) is compared with the home directory and
  the data directory, as real paths. If a mount source is one of these
  directories or a parent of them, the start fails. For `/home/me`, the
  sources `/home/me`, `/home` and `/` fail. The error names the mount and
  the setting that turns the check off.
- New user setting `[security] insecure_mounts` (default false) turns the
  check off.
- Mounts below the protected directories stay allowed, for example a
  project in `~/src` and the pack mounts in `<data>/packs/mounts/`. File
  mounts are not checked: they share one file that the user selected.
- The `.airlock` mask of a sandbox in the project stays as it was.

## Alternatives

- A first version hid `~/.airlock` and the data directory in the guest
  with read-only bind mounts over them (new `hiddenDirs` boot field). It
  was complex, and a home mount also gives the guest all other secrets,
  for example `~/.ssh`. Guest root could also unmount the mask. The start
  check protects all of the home directory, and it is all on the host.

## Tests

- `vm/mount.rs`: the home, its parents, `/`, the data directory and a
  symlink to the home are refused. A project below the home, a pack mount
  and a file mount of the home are allowed.
- VM bats: the tests now use a home directory next to the project, not
  the project itself. `tests/vm/mounts.bats` checks that a project in the
  home directory is refused, and starts with `insecure_mounts = true`.
  Not run here (no KVM).
