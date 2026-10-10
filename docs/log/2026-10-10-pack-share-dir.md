# Pack mounts move to `share/all/packs`

The host side of the pack mounts moves from `<data>/packs/mounts/<name>/`
to `<data>/share/all/packs/<name>/`.

- `share/` holds the directories that the host shares with sandboxes.
- `all/` holds the directories that all sandboxes share. A later
  change can add a sibling for directories that only the sandboxes of
  one project share.
- `cache::pack_mounts_dir` makes the new path. The pack sees it as
  `pack.directory`.

The old path was not released. Thus there is no migration.
