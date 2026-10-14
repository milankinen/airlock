# I/O security

Code in the sandbox can write some host directories: the project
directory, `.airlock/` and read-write mounts. It can put symlinks, FIFOs
and hard links there, or swap a directory for a link. A link from one
session must not take effect in a later session.

To protect the host from these links, airlock uses the `VerifiedPath`
type for all host file I/O. A `VerifiedPath` starts at a trusted root:
the home directory, the project directory or the data directory. Airlock
refuses the path when a part below the root is a symlink, or when the
path contains `..`. Parts that do not exist yet are allowed. Airlock
checks the path again before each file operation, so a link or a swapped
directory causes an error.

At the last part of the path:

* A symlink or a FIFO is refused. Sockets and devices are allowed.
* Remove, rename and link inspection act on the link itself and do not
  follow it. Thus `airlock rm` can remove a symlinked `.airlock`.

## Links that airlock follows

Airlock follows links only where the user picks the path:

* dotfile config
* mount sources
* programs in `PATH` and the airlock binary
* a data directory that the user moved

## Special cases

* **Mount sources, `vm.kernel` and `vm.initramfs`:** a path that goes
  into a directory that the sandbox can write must stay in it. Airlock
  stops with a hint to set the real path.
* **Host sockets:** a relative host path resolves from the project
  directory. A socket in a directory that the sandbox can write must stay
  in it. Airlock pins it at start and refuses a connection to a changed
  socket or to a socket with more than one link. Sockets in other
  directories, for example `/var/run/docker.sock`, are not pinned, so a
  host service can restart.
* **Config change check:** airlock compares the resolved host paths with
  the last start. If a link behind `source = "./data"` changes, airlock
  asks the user and shows the old and the new path.
* **File sync:** airlock opens the directories one time and does the file
  operations relative to them. A later swap of a directory has no effect.
  A symlink or a FIFO in the sandbox copy is not followed.

## Accepted risk

A check and the file operation after it are not one atomic step. While a
VM runs, the sandbox can swap a directory between the two. The check
before each operation keeps this window short. Full protection would
need handle-based I/O for all paths, which adds much complexity.

## Enforcement

Clippy refuses plain path types, the standard file system calls, path
probes, Unix socket bind and connect, `symlink` and `chown` in the host
code. Only a short list of files can allow these calls.
