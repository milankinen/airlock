# Project layout

## Data directory

airlock keeps its shared data and the sandboxes in one data directory.
The default is `~/Library/Application Support/airlock` on macOS and
`$XDG_DATA_HOME/airlock` (or `~/.local/share/airlock`) on Linux. The
`data_dir` setting in `~/.airlock/settings.toml` changes it.

```
<data>/
  db/                            # airlock database (LMDB)
  boxes/
    <id>/                        # one sandbox (see below)
  vm/
    Image                        # Linux kernel (extracted on first run)
    initramfs.gz                 # initramfs
    cloud-hypervisor             # (Linux only) hypervisor binary
    virtiofsd                    # (Linux only) VirtioFS daemon
    checksum                     # triggers re-extraction after binary update
  oci/
    images/<digest>              # schema-tagged JSON: the fully-baked OciImage
    layers/<digest>/             # extracted layer tree (whiteouts as xattrs)
    layers/<digest>.download.<pid>.<seq>.tmp
                                 # in-flight download, one per process (swept on next run)
    layers/<digest>.download     # complete tarball pending extraction
    layers/<digest>.tmp/         # in-flight extraction (swept on next run)
  templates/
    <name>/                      # one sandbox template (see below)
    .build-<name>/               # build directory of a template (temporary)
  share/all/packs/<pack>/        # host side of the pack mounts
  sock/<hash>.sock               # CLI socket when the default path is too long
```

User-level state stays in `~/.airlock/`:

```
~/.airlock/
  settings.toml                  # application settings (vault, data_dir)
  airlock.toml, config.toml      # user config
  vault.*                        # file vault backends
```

The `sandboxes` database in `db/` maps each sandbox id to its canonical
project directory. It is the registry of the sandboxes in `boxes/`.
`airlock sandboxes list` shows it. The id is 8 random lowercase base32
characters.

## Template directory

A template directory is `<data>/templates/<name>/`. See
[Templates](../templates.md). It holds only these files:

```
<template>/
  airlock.toml                   # wizard config, vm.image pinned to name@<digest>, if known
  disk.img                       # disk with the installed packs
  disk.id                        # identity of disk.img
  installs.json                  # pack install records
  ca.json                        # CA keypair of the build sandbox
  image                          # link to the cached image
```

The `image` link keeps the cached image from cleanup. A build runs in
`<data>/templates/.build-<name>/`. After a failed build, only the install
log (`sandbox/installs.log`) stays there.

## Sandbox directory

A sandbox directory is `<data>/boxes/<id>/`. The registry in the data
directory maps each project to its sandbox. The sandbox directory
holds:

```
<sandbox>/
  ca.json                        # CA certificate + key PEMs (single JSON file)
  run.json                       # last-run timestamp, guest_cwd, config files of the last start
  installs.json                  # pack install records
  airlock.toml                   # local project config
  overlay/
    files/rw/{key}               # hard-linked writable file mounts
    files/ro/{key}               # hard-linked read-only file mounts
  disk.img                       # virtio-blk ext4 volume
  disk.id                        # random identity of disk.img
  lock                           # flock: one airlock process per sandbox
  cli.sock                       # Unix socket for `airlock exec` RPC
  image                          # hard link to images/<digest> JSON
  airlock.log                    # tracing log of the last runs
  installs.log                   # output of the last install boot
```

The sandbox directory is never in the project. The sandbox guest cannot
write it.

airlock writes the JSON files atomically (temp file and rename) under the
sandbox lock. Other processes (`airlock sandboxes list`, `airlock info`)
read them at the same time without a wait.

The local project config (`airlock.<ext>`) is the file that the setup
wizard writes when `Create airlock.toml` is off. `airlock start` saves it
in the sandbox directory. Thus it is never in the repository. An older
version, or the user, can leave it in `<project>/.airlock/`. The first
start moves it to the sandbox directory and removes `.airlock/`. If
`.airlock/` also holds other files, the start stops with exit code 1 and
changes nothing:

```
<project>/
  airlock.toml                   # project config (tracked by VCS)
  .airlock/                      # only from an older version or the user
    airlock.toml                 # local project config (optional)
```

`airlock rm` uses only the registry. It removes the registry entry, the
sandbox directory and the local project config. It does not touch
`<project>/.airlock/`. If a stale `.airlock/sandbox` exists, the next
`airlock start` offers to import it. If the project has no sandbox, it
prints "No sandbox to remove" and exits with 0. A running sandbox gives
exit code 1.

### Import of an old sandbox

Older versions kept the sandbox in `<project>/.airlock/sandbox/`. The
guest can write that directory, so airlock does not trust its content.
A registered sandbox wins, and airlock then ignores `.airlock/sandbox`.
A symlinked `.airlock` gets no import offer, and `start` makes a new
sandbox. The import follows these rules:

* airlock checks in this order:
  1. It refuses a non-directory or a symlink at `.airlock/sandbox`.
  2. It refuses when `.airlock` holds names other than `.gitignore`,
     `airlock.log`, `sandbox` and `airlock.<ext>`. It exits with 1 and
     changes nothing.
  3. It pins the directory without a link follow and checks the owner.
  4. It takes `lock`. Only a missing `lock` lets the import go on. A held
     lock means the sandbox is in use. A symlink or FIFO at `lock` is
     refused.
* airlock takes only these names: `disk.id`, `disk.img`, `installs.json`,
  `ca.json` and the image digest from `image`. It does not take any other
  entry.
* airlock opens each entry without a link follow. The entry must be a
  regular file with one link, owned by the user. It skips other entries.
  `image` is the exception. It needs no one-link check, because airlock
  reads only a `sha256:<64 hex>` digest from it.
* airlock takes `disk.img` only with a valid `disk.id` (32 hex characters).
  airlock renames the file, or on another file system it makes a sparse
  copy.
* airlock takes `installs.json` and `ca.json` only with the disk. They must
  parse.
* airlock sets `ran_session` to true in the install records. All records
  stay. Records that are not `installed` or `kept` become `failed`.
* airlock does not take `run.json` and the `image` file. When the image
  is in the cache, airlock links it at once. When airlock took the disk,
  it also writes the marker `image.imported` with the old digest, or
  empty if the digest is unknown. The next start uses the marker as the
  stored digest:
  * A different digest gives the image change question.
  * An empty marker counts as a change for all images. With a disk,
    `--yes` does not re-create the sandbox. The start stops with exit
    code 2 and the disk stays.
  * airlock removes the marker after the next image step that succeeds.
* After an error, airlock removes the new sandbox directory and puts back
  the moved files. The old directory stays. If the disk cannot go back,
  airlock keeps the new sandbox and its registry entry, and the error
  names the disk path.
* After a successful import, airlock moves the local project config and
  removes all of `.airlock/`. A failed remove is only a warning.

## CA keypair

On the first `airlock start`, airlock generates a self-signed CA keypair
and writes it to `ca.json` in the sandbox directory, a JSON object with
`cert` and `key` PEM fields. airlock reads the PEM strings into memory
one time and keeps them on the `Project` struct. The `start` RPC gives
the PEM bytes to the guest, and the guest injects them after it mounts
overlayfs (see [Mounts / CA certificate injection](./mounts.md#ca-certificate-injection)).

## Image cache

All projects share the image cache. Layers are
content-addressable by digest, so two images that share a base layer
extract it only once. The platform follows the host architecture:
`linux/arm64` on ARM hosts, `linux/amd64` on x86_64 hosts.

Each `images/<digest>` entry is a single JSON file with the serialized
`OciImage` (in a `{"schema":"v2", …}` envelope for forward-compatible
schema changes). airlock writes it atomically with a `.tmp` rename, and
then hard-links it into the sandbox at `image`. A link count greater
than 1 on the cached file means that at least one sandbox uses the image.
This prevents GC. The hard link needs the sandbox and the cache on one
file system.

A `<digest>/` layer directory only exists through the atomic rename
from `<digest>.tmp/`. Thus its presence is the completion marker, and no
separate `.ok` file is necessary.

The sandbox `image` file has two purposes. It is the GC reference of the
sandbox, and it is the stored-image source: it gives the full cached
`OciImage`, with the digest that shows image changes across runs. When
the digest changes, the guest resets the overlay upper layer.

## Locking

`lock` holds an exclusive `flock` while `airlock start` runs, and
contains its PID for diagnostics. A second `airlock start` on the same
sandbox fails at once. `airlock rm` and `airlock sandboxes remove` probe
the lock without a wait, and refuse a running sandbox. The kernel
releases the lock when the process exits, so there are no stale locks.
