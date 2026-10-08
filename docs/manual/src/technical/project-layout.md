# Project layout

## Sandbox directory

Each project stores its sandbox state locally in `.airlock/` next to
the config file. airlock writes a `.gitignore` containing `*` there
automatically, so version control tracks nothing under `.airlock/`.

```
<project>/
  airlock.toml                   # user config (tracked by VCS)
  .airlock/
    .gitignore                   # contains "*" — auto-created
    airlock.toml                 # local project config (optional)
    airlock.log                  # tracing log from the last run
    sandbox/
      ca.json                    # CA certificate + key PEMs (single JSON file)
      overlay/
        files/rw/{key}           # hard-linked writable file mounts
        files/ro/{key}           # hard-linked read-only file mounts
      disk.img                   # virtio-blk ext4 volume
      run.json                   # last-run timestamp, guest_cwd
      lock                       # PID lockfile (one VM per project)
      cli.sock                   # Unix socket for `airlock exec` RPC
      image                      # hard link to images/<digest> JSON
      installs.json              # pack install records (see Packs)
      installs.log               # pack install output (8 MiB cap)
```

airlock creates `.airlock/` with mode 0700. It refuses a symlinked or
foreign-owned `.airlock/`.

`airlock rm` removes the entire `.airlock/` directory, also the local
project config. `airlock.toml` and `airlock.local.toml` are untouched.
If the project is a home directory, or if `.airlock/` holds user-level
files (vault, `db/`, `settings.*`, `config.*`), `airlock rm` removes
only `.airlock/sandbox/`. A symlinked `.airlock` is only unlinked.

The CA is a single file — there is no longer a `sandbox/ca/`
directory. The `boot` RPC passes the PEM bytes read from `ca.json`
to the guest, and the guest injects them after mounting
overlayfs (see [Mounts / CA certificate injection](./mounts.md#ca-certificate-injection)).

## CA keypair

On first `airlock start`, airlock generates a self-signed CA keypair
and writes it to `sandbox/ca.json` as a JSON object with `cert` and
`key` PEM fields. It reads the PEM strings into memory once and keeps
them on the `Project` struct — no further file reads are needed at
TLS setup or guest CA injection.

## Global cache

```
~/.cache/airlock/
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
  packs/mounts/<pack>/           # host side of pack mounts, shared by projects
```

User-level state lives in `~/.airlock/`:

```
~/.airlock/
  settings.toml                  # application settings (vault, wizard defaults)
  airlock.toml, config.toml      # user config
  vault.*                        # file vault backends
  db/                            # LMDB database (network service sign-ins)
```

The image cache is shared across all projects. Layers are
content-addressable by digest, so two images that share a base layer
extract it only once. The platform follows the host architecture:
`linux/arm64` on ARM hosts, `linux/amd64` on x86_64 hosts.

Each `images/<digest>` entry is a single JSON file carrying the
serialized `OciImage` (wrapped in a `{"schema":"v2", …}` envelope for
forward-compatible schema evolution). airlock writes it atomically via
`.tmp` rename and then hard-links it into the sandbox at
`sandbox/image`. A link count greater than 1 on the cached file means
at least one sandbox references the image, which prevents GC.

A `<digest>/` layer directory only exists through the atomic rename
from `<digest>.tmp/`, so its presence is itself the completion marker
— no separate `.ok` file is needed.

`sandbox/image` serves two purposes: it's the per-project GC
ref, and it's the stored-image source — reading it as JSON gives the
full cached `OciImage`, including the digest used to detect image
changes across runs. When the digest changes, the guest resets the
overlay upper layer.

## Locking

`sandbox/lock` contains the running PID. If the lock file exists and
the PID is alive, `airlock start` refuses to start (one VM per
project at a time). `airlock rm` takes the same lock during the
removal. airlock silently clears stale locks (dead PID).
