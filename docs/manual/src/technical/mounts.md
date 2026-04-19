# Mounts

## How VirtioFS shares work

Each mount becomes a [VirtioFS](https://virtio-fs.gitlab.io/) share
(one virtio device per share). Guest init mounts each share under
`/mnt/<tag>`, and the supervisor then bind-mounts from `/mnt/<tag>`
into the container rootfs after the overlayfs is composed.

The full set of shares present at boot:

| Tag               | Host path                          | Mode |
|-------------------|------------------------------------|------|
| `base`            | `~/.cache/airlock/oci/layers/…`    | ro   |
| `project`         | project CWD                        | rw   |
| `dir_0`, `dir_1`… | each `[mounts.*]` directory mount  | configurable |
| `files_rw`        | `.airlock/sandbox/overlay/files/rw/` | rw |
| `files_ro`        | `.airlock/sandbox/overlay/files/ro/` | ro |

`files_rw` / `files_ro` are only created when the project has at least
one read-write / read-only file mount.

## Project directory

Always mounted at the same absolute path as on the host. This means
paths in build tools, error messages, and scripts are identical inside
and outside the sandbox. The container shell's working directory is
set to this path.

## Directory mounts

A VirtioFS share pointing directly at the host directory. Read-only or
read-write as configured. Dir mounts are sorted by config key and
assigned tags `dir_0`, `dir_1`, … so the tag→host mapping is stable
across boots of the same config.

## File mounts

File mounts want to expose a *single host file* at a chosen path
inside the container, with writes syncing back to the host. Getting
this to work on VirtioFS took a few iterations — the final design has
enough moving parts that it's worth explaining why each piece exists.

### Why not VirtioFS file-level bind mounts

The obvious approach — bind-mount the VirtioFS-exposed file directly
at its target path — does not work. `stat` and `ls` succeed, but reads
inside the container fail with `EACCES` regardless of uid, mode, or
capabilities. Directory bind mounts over VirtioFS are fine; file bind
mounts are the broken case. This is a VirtioFS/FUSE limitation, not
something we can fix host-side.

So file mounts need an indirection: the container has to see a
directory-level bind mount, and the expected file path has to resolve
into it.

### Why hard links into a staging directory

Files for a project come from arbitrary locations scattered across the
host filesystem. We can't expose each one as its own VirtioFS share
(one virtio device per share burns device slots fast) and we can't
point a single share at many different parent directories.

The fix: one staging directory per mode, and each mount is hard-linked
from its source into that staging dir under a unique key:

```
.airlock/sandbox/overlay/files/rw/
  claude-json      ← hard link to ~/.claude.json
  mise-toml        ← hard link to <project>/mise.toml
```

Hard links share the inode with the source, so edits made inside the
container appear on the host and vice versa without any copying. The
two staging dirs (`rw/` and `ro/`) get wrapped as the `files_rw` and
`files_ro` VirtioFS shares; all file mounts ride a single device each.

If hard-linking fails (cross-filesystem `EXDEV` — happens when the
project and the sandbox state live on different filesystems, e.g.
project on VirtioFS inside a nested VM, sandbox state on ext4) the
file is copied instead with a warning that sync becomes one-way. This
is unavoidable — a hard link can't cross filesystem boundaries.

### Why symlinks in the upperdir

Each file mount has to appear at its user-chosen target path — e.g.
`~/.claude.json` or `/etc/app/config.json`. The container rootfs is a
composed overlayfs with the image layers on the bottom and a persistent
upperdir on the project disk. Before mounting the overlay, guest init
writes a symlink into the upperdir at the target's relative path:

```
upper/root/.claude.json  →  /airlock/.files/rw/claude-json
upper/etc/app/config.json →  /airlock/.files/rw/app-config
```

When the overlay is mounted, the symlink is merged in at its target
path. A read on `~/.claude.json` inside the container follows the
symlink to `/airlock/.files/rw/claude-json`, which is the
directory-level bind mount of `/mnt/files_rw` — a VirtioFS directory,
where file-level access does work.

The full write path:

```
container: write ~/.claude.json
  → symlink in overlayfs upperdir
  → /airlock/.files/rw/claude-json           (bind mount)
  → /mnt/files_rw/claude-json                (VirtioFS)
  → overlay/files/rw/claude-json             (host, hard-linked)
  → ~/.claude.json                            (original source inode)
```

An earlier design used per-file bind mounts applied *after* the
overlay was composed, but that broke whenever a file mount's target
fell under a directory mount — the directory bind mount would cover
the file-mount target. Putting the indirection symlinks into the
upperdir *before* any other mount runs means directory mounts can sit
on top without hiding file mounts, and file mounts can still target
paths inside mounted directories cleanly.

## CA certificate injection

The project CA certificate (used for TLS interception) is delivered to
the guest via the `caCert` field on the `start` RPC. Guest init builds
a **tmpfs lowerdir** at `/mnt/ca-overlay` containing per-distro CA
bundle files with the project CA appended, and splices that tmpfs on
top of the image layers in the overlayfs `lowerdir` stack:

```
lowerdir=/mnt/ca-overlay:/mnt/layers/<top-digest>:…:/mnt/layers/<bottom-digest>
```

Because the CA layer is a tmpfs sitting *below* the overlay, the
injected bytes never land on the persistent upperdir — on the next
boot the same injection runs again against the pristine image content,
so there's no accumulation of duplicated cert blocks.

For each known bundle path (Debian/Ubuntu, Alpine, RHEL/Fedora,
openSUSE, Arch), the guest walks the image layers topmost-first, takes
the first non-empty copy of that bundle, appends the project CA, and
writes the result into the tmpfs at the same relative path. If no
layer ships any bundle, the CA is written at
`etc/ssl/certs/ca-certificates.crt` as a fallback so `SSL_CERT_FILE`
can point at a predictable location.

The raw CA is also dropped at every well-known anchor directory
(`usr/local/share/ca-certificates/airlock.crt`, etc.) so distro
trust-update tools (`update-ca-certificates`, `update-ca-trust`,
`trust extract-compat`) regenerate bundles that still include it.
