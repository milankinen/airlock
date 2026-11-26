# Treat `boxes/` as an internal dir of the data dir

airlock followed a symlink at `<data>/boxes`, so that a user could move
the sandboxes to another disk. The manual did not tell about it, and no
test used it.

The setup also did not work: the `image` hardlink of each sandbox must
be on the same file system as `oci/images/`, so a start with `boxes/` on
another disk failed.

## Change

- `DataDir.sandboxes` is now `<data>/boxes` with the normal path check.
  A symlink at `boxes` is refused, the same as for the other dirs in
  the data dir.
- To move all airlock data, the user moves the data dir with the
  `data_dir` setting or a link. That stays supported.
