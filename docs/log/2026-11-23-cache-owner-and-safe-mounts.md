# Cache owner and safe mount targets

## Why

* Named cache dirs belonged to root. A non-root image user could not
  write in them, for example in `~/.cache`.
* The guest can make symlinks that steer the mounts of the init process,
  which runs as root. Known attack paths:
  * A `..` in a symlink target escapes the container root.
  * A symlink in a shared host dir (project dir, dir mounts) points a
    mount target to an image dir.
  * A symlink that the user makes in the persistent upper layer points
    a root mount to another place.
  * A check followed by a mount has a race (TOCTOU). The guest changes
    the path between the two steps.

## Design

* Mount targets resolve with a walk that opens each name with `openat`
  and `O_NOFOLLOW`. The walk holds the fds. It does not use `openat2`
  with `RESOLVE_IN_ROOT`, because the walk must apply a policy to each
  link.
* The walk handles `..` itself and clamps it at the root.
* Image symlinks are followed. A policy table decides on the other
  links. A link in a shared host dir is refused. A link in a dir that
  the non-root container user can write is refused. With uid 0 the
  writable check does not apply.
* A refused cache or dir mount gives a warning, and init skips it. A
  refused mask stops the boot. Masks fail closed.
* The mount uses the held fds: `open_tree`, `mount_setattr` and
  `move_mount`. A read-only remount through `/proc/self/fd` changes the
  mount below the target, so it is not usable.
* Owner of a named cache dir:
  * The target exists in the image: the cache dir gets its uid, gid and
    mode.
  * The target does not exist: the container uid and gid, mode 0755. The
    new target dir also gets the container user.
  * New parent dirs get the owner of the nearest existing parent. For
    example `~/.cargo` under a user-owned `~` belongs to the user.
  * Only a new or empty cache dir changes owner. A cache that an older
    version made as root, and that has content, stays root-owned. Remove
    the cache to get the new owner.
* The writable check counts a dir of the container user as writable for
  each mode, because the owner can `chmod` it. Container processes have
  no supplementary groups, so the check uses only the primary gid.
* A file in a mount path is refused like a link: the container user or a
  share can put it there, and a hard error stops each next boot.
* Mask paths resolve below the project dir, so `..` stays in the project.
  A guest can stop the next boot of its project with a link at a mask
  path. This is accepted, because masks fail closed.
* A new dir gets a chown only if its owner differs from the parent
  owner. A chown failure gives a warning, because a share can refuse it.

## Out of scope

* A file mount through a symlink still creates the parent dirs with a
  path-based `create_dir_all` as root, in the upper layer.
* The codex pack cache in `~/.codex/tmp` is in a shared dir. A symlink
  there makes init skip the cache. Not yet checked in a VM.

## Testing

* VM tests in `tests/vm/caches.bats`: cache owner with a non-root user,
  and a skipped cache path with a symlink in the project dir.
