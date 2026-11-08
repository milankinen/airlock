# Layer cache keeps image owners for libkrun

## Problem

The layer extraction on the host drops the owners of the tar entries. airlock
runs as the normal host user, and macOS does not let a normal user `chown`.
The `tar` crate default also keeps only the `0o777` mode bits, so setuid,
setgid and sticky bits are lost. All backends show the rootfs files as
root. For example, in a `node` image `/home/node` is owned by root, and
the `node` user cannot write its home dir from the image.

## Fix

libkrun's macOS passthrough reads the guest owner and mode of a file from
the xattr `user.containers.override_stat` (`<uid>:<gid>:<octal mode>`). An
empty uid or gid field keeps the owner of the host file. The extractor now
writes this xattr for each regular file and directory that:

* has an owner other than `0:0`, or
* has a setuid, setgid or sticky bit.

Root-owned entries without special bits get no xattr. Owner 0 is written
as an empty field: the host file owner then shows through, and the guest
idmap of the `layers` share maps it to root. A literal 0 would be swapped
to the host uid by that idmap.

Examples: `/home/node` gets `1000:1000:755`, `/usr/bin/sudo` gets
`::4755`.

Details:

* libkrun parses each field on its own. An empty field does not parse, so
  libkrun keeps the host value for it and still uses the other fields.
* Hard links, symlinks, FIFOs and device entries get no xattr. The `tar`
  crate makes FIFO and device entries as regular files. A hard link shares
  the inode of its target entry. Owners of symlinks have no effect.
* The host refuses a user xattr on a file without write permission (Linux
  always, macOS for most files). The extractor sets the owner write bit for
  the change and then restores the mode.
* An entry below a symlink in the layer (`lib -> usr/lib`, then
  `lib/x`) gets no xattr. `unpack_in` follows such symlinks inside the
  root, but a change through a symlink could reach a host file. Such
  entries are rare: images put files at the real path. A debug log names
  them.
* macOS gives a new file the group of its directory, for example `wheel`
  below `/tmp`. The `layers` idmap swaps the user's gid with 0, so a
  `wheel` file showed as gid 20 in the guest. The extractor sets the group
  of the staging dir to the user's gid, and the files inherit it.
* Tar uses "last entry wins". A later root-owned entry for an existing
  directory keeps the directory, so the extractor removes an older owner
  xattr from it.
* A host filesystem without user xattrs (`ENOTSUP`) only loses the owners.
  Only libkrun reads them, and libkrun runs on macOS.
* A tar header with a bad uid or gid counts as root. A bad mode gives no
  xattr. The extraction itself does not need these fields, so they must
  not stop it.

## Other backends

Apple Virtualization and virtiofsd do not read this xattr. For them it is
an extra user xattr in the guest, and overlayfs copies it on copy-up. Their
behavior does not change.

## Cache invalidation

`LAYER_FORMAT` goes from 2 to 3, and the image JSON schema from `v2` to
`v3`. New runs ignore the old layer dirs and image files and pull again.
`gc` removes the old dirs later.

Effect on existing sandboxes:

* The sandbox image file is a `v2` file, so the cache read finds no ready
  image, and the fast path of `if-not-present` does not apply. The first
  start needs the registry (or the local engine) one time to pull the
  layers again. The disk and the overlay upper layer stay.
* The image change check reads only the digest of the old file, from
  `v2` or `v3` JSON.
* For `if-not-present`, airlock pulls the stored digest
  (`<name>@<digest>`), not the tag. Without this, a tag that moved since
  the last start looks like an image change: a question, a re-create with
  `--yes`, or a refusal without a terminal. The same applies when the
  sweep removed the layers of a sandbox. The pinned try uses only
  anonymous access and the stored credentials, and never asks. On any
  error it falls back to the tag. This covers local images, whose stored
  digest is a local image ID that no registry knows. With
  `resolution = "docker"` or `"podman"` there is no pinned try, because it
  can only fail. With `auto`, a local-only image gives a failed pinned try
  (two "trying next" lines and one registry request), then the tag.
* The legacy sandbox import reads the digest the same way.

Old compatibility code that the bump makes unreachable is removed:

* Image files without the `user` field (written before named `USER`
  resolution, PR #12) were checked again against the image passwd. Such
  files are `v2`, so they no longer load. `user` is now a required field.
  A sandbox from before PR #12 with a named non-root `USER` now starts as
  that user with no check, also when its disk has root-owned state.
* The removal of the fixed-name download tmp file of older binaries.

## Known limits

* `uid`/`gid` come from the tar header. PAX records for ids above 2097151
  are not read, so such entries count as root. They are rare.
* The `layers` idmap swaps the host ids with 0. So an image entry owned by
  the host uid (for example 501 on macOS) shows as root. Such entries are
  rare in images.
* The same holds for gids. The host gid is often 20 (`staff`), which is
  `dialout` in Debian images. Image files with gid 20 are rare.
