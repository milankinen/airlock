# Project layout

## Sandbox directory

Each project stores its sandbox state locally in `.airlock/` next to
the config file. A `.gitignore` containing `*` is written there
automatically so nothing under `.airlock/` is tracked by version
control.

```
<project>/
  airlock.toml                   # user config (tracked by VCS)
  .airlock/
    .gitignore                   # contains "*" — auto-created
    airlock.log                  # tracing log from the last run
    sandbox/
      ca.json                    # CA cert + key PEMs (single JSON file)
      overlay/
        files/rw/{key}           # hard-linked writable file mounts
        files/ro/{key}           # hard-linked read-only file mounts
      disk.img                   # virtio-blk ext4 volume
      run.json                   # last-run timestamp, guest_cwd
      run.log                    # tracing log from the last run
      lock                       # PID lockfile (one VM per project)
      cli.sock                   # Unix socket for `airlock exec` RPC
      image                      # hard link to images/<digest> JSON
```

`airlock rm` removes the entire `.airlock/` directory. The config
file is untouched.

The CA is a single file — there is no longer a `sandbox/ca/`
directory. The PEM bytes read from `ca.json` are passed to the guest
via the `start` RPC and the guest injects them after mounting
overlayfs (see [Mounts / CA certificate injection](./mounts.md#ca-certificate-injection)).

## CA keypair

On first `airlock start`, a self-signed CA keypair is generated and
written to `sandbox/ca.json` as a JSON object with `cert` and `key`
PEM fields. The PEM strings are read into memory once and kept on the
`Project` struct — no further file reads are needed at TLS setup or
guest CA injection.

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
    layers/<digest>.download.tmp # in-flight download (swept on next run)
    layers/<digest>.download     # complete tarball pending extraction
    layers/<digest>.tmp/         # in-flight extraction (swept on next run)
```

The image cache is shared across all projects. Layers are
content-addressable by digest, so two images that share a base layer
extract it only once. Platform is fixed to `linux/arm64` (matching
the VM architecture).

Each `images/<digest>` entry is a single JSON file carrying the
serialized `OciImage` (wrapped in a `{"schema":"v1", …}` envelope for
forward-compatible schema evolution). It is written atomically via
`.tmp` rename and then hard-linked into the sandbox at
`sandbox/image`; a link count greater than 1 on the cached file means
at least one sandbox references the image, which prevents GC.

A `<digest>/` layer directory only exists through the atomic rename
from `<digest>.tmp/`, so its presence is itself the completion marker
— no separate `.ok` file is needed.

`sandbox/image` serves two purposes: it's the per-project GC
ref, and it's the stored-image source — reading it as JSON gives the
full cached `OciImage`, including the digest used to detect image
changes across runs. When the digest changes the overlay upper layer
is reset.

## Locking

`sandbox/lock` contains the running PID. If the lock file exists and
the PID is alive, `airlock start` refuses to start (one VM per
project at a time). Stale locks (dead PID) are silently cleared.
