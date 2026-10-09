# Move the old cache into the data directory

Review finding #3 (`FINDINGS.md` in faa033a).

## Problem

v2026.9.6 kept the image cache in `~/.cache/airlock` (`oci/`, `vm/`,
`sock/`). The data directory change moved it to the platform data
directory with no migration. The log entry of that change said the old
location was not released. That was wrong.

After an upgrade, an existing sandbox did not find its image in the
cache, so `airlock start` asked the registry for the digest. If the tag
had moved (for example `alpine:latest`), the user got "image changed".
The default answer and `--yes` re-create the sandbox and erase
`disk.img`. Without a terminal, or offline, the start failed. The old
cache stayed on disk with no cleanup.

## Change

- `cache::migrate_legacy_cache` runs in `Context::load`, before the data
  directory is set and the database opens. Each command runs it, so the
  first command after the upgrade moves the cache. The user sees
  nothing.
- It runs only if `~/.cache/airlock` exists and the data directory does
  not. Then it renames the whole directory to the data directory and
  sets mode 0700. There is nothing to merge. The cache has no symlinks
  of its own. Symlinks that a sandbox made in pack mounts move as they
  are.
- One rename, no copy. A rename keeps the inodes:
  - The `image` hard link of a sandbox still shares the inode with
    `oci/images/<digest>`, so GC protection still works.
  - Running VMs of the old version keep their open files.
- If the rename fails (for example `EXDEV`: a `data_dir` setting on
  another file system), airlock shows a warning and uses
  `~/.cache/airlock` as the data directory. The warning shows on each
  run until the user moves the directory. A copy of gigabytes of layers
  with their overlay xattrs at start is not worth it.
- A first version merged the old cache into an existing data directory,
  entry by entry. It was too complex for a directory that does not
  exist before the upgrade.

## Tests

- `cache.rs`: the move gives a private data directory with the old
  entries, and the old directory is gone.
- Manual: `airlock sandbox list` with a temporary `HOME` moved
  `~/.cache/airlock/oci/images/abc` and removed the old directory.
