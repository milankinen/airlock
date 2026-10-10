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
  packs/mounts/<pack>/           # host side of the pack mounts
  sock/<hash>.sock               # CLI socket when the default path is too long
```

User-level state stays in `~/.airlock/`:

```
~/.airlock/
  settings.toml                  # application settings (vault, wizard defaults, data_dir)
  airlock.toml, config.toml      # user config
  vault.*                        # file vault backends
```

The `sandboxes` database in `db/` maps each sandbox id to its canonical
project directory. It is the registry of the sandboxes in `boxes/`.
`airlock sandbox list` shows it. The id is 8 random lowercase base32
characters.

## Sandbox directory

A sandbox directory is `<data>/boxes/<id>/` (a managed sandbox). With
`sandbox_type = "project-owned"` in the `[security]` settings, or for a
sandbox that an older airlock made, it is `<project>/.airlock/sandbox/`. Such a sandbox is not
in the registry. Both kinds have the same contents:

```
<sandbox>/
  ca.json                        # CA certificate + key PEMs (single JSON file)
  run.json                       # last-run timestamp, guest_cwd
  installs.json                  # pack install records
  keep-in-project                # (project sandbox only) do not offer a move
  airlock.toml                   # (data directory only) local project config
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

airlock writes the JSON files atomically (temp file and rename) under the
sandbox lock. Other processes (`airlock sandbox list`, `airlock info`)
read them at the same time without a wait.

The local project config (`airlock.<ext>`, for example from the `start`
option of the setup wizard) is in the sandbox directory of a sandbox in
the data directory. Thus it is never in the repository. A project sandbox
keeps it in `.airlock/`:

```
<project>/
  airlock.toml                   # project config (tracked by VCS)
  .airlock/                      # project sandbox only
    .gitignore                   # contains "*" — auto-created
    airlock.toml                 # local project config (optional)
    airlock.log                  # tracing log (older airlock versions)
    sandbox/                     # the sandbox (see above)
```

A move of a project sandbox into the data directory is one `rename`.
Thus the `image` hard link stays valid. The move also moves the local
project config into the new sandbox directory and then removes
`.airlock/`. If `.airlock/` holds any other file, airlock refuses the move
and changes nothing. A new sandbox in the data directory also takes the
local project config out of `.airlock/` in the same way. If the user
chooses to keep a project sandbox in the project, airlock writes the
`keep-in-project` marker, and `airlock start` does not offer the move
again.

For a project sandbox, the guest masks `<project>/.airlock` with an
empty read-only directory (the `boot` RPC tells the guest the sandbox
location). A sandbox in the data directory gets no mask. Its data and its
local project config are not in the project.

`airlock rm` removes the sandbox directory of the project. For a sandbox
in the data directory, it also removes the registry entry and the local
project config. For a sandbox in the project, it removes the entire
`.airlock/` directory.
If the project is a home directory, or if `.airlock/` holds user-level
files (vault, `settings.*`, `config.*`), `airlock rm` removes only
`.airlock/sandbox/`. A symlinked `.airlock` is only unlinked.

airlock creates `.airlock/` and the sandbox directory with mode 0700. It
refuses a symlinked or foreign-owned `.airlock/`.

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
sandbox fails at once. `airlock rm` and `airlock sandbox remove` probe
the lock without a wait, and refuse a running sandbox. The kernel
releases the lock when the process exits, so there are no stale locks.
