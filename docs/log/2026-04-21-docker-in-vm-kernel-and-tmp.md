# Make Docker-in-VM actually work: kernel config + /tmp tmpfs

## What

Three separate fixes, in one commit, all discovered while debugging why
`docker build` of a standard multi-stage image failed inside the airlock
sandbox (Option 2 of `docs/manual/src/tips/docker.md` — running `dockerd`
inside the VM).

1. **Kernel config**: several options Docker/containerd/BuildKit rely on
   were missing from the VM kernel. Added to both `config-x86_64` and
   `config-arm64`.
2. **ext4 xattr handlers on arm64**: `CONFIG_EXT4_FS_SECURITY` and
   `CONFIG_EXT4_FS_POSIX_ACL` were only in the x86_64 config; the arm64
   defconfig-style config never explicitly enabled them, so arm64 kernels
   shipped without `security.*` xattr support on ext4.
3. **airlockd**: `/tmp` inside the container is now its own tmpfs instead
   of a directory on the outer overlayfs rootfs.

## Why it mattered — the three failures

### (a) `iptables … -j DNAT` failed

`docker run -p 8000:8000 …` produced:

> `Warning: Extension DNAT revision 0 not supported, missing kernel module?`
> `iptables v1.8.11 (nf_tables): RULE_APPEND failed`

The kernel had `CONFIG_NF_NAT=y` and `CONFIG_NFT_NAT=y`, but not
`CONFIG_NETFILTER_XT_NAT`, which is what provides the `DNAT`/`SNAT`
targets for the xtables compat layer that both iptables-legacy and
iptables-nft use. Without it, `iptables -j DNAT` is an unknown target.

Added: `NETFILTER_XT_NAT`, plus a handful of other xtables
targets/matches Docker's chain templates reference (`REJECT`,
`multiport`, `comment`, `owner`).

### (b) BuildKit layer export: `EOPNOTSUPP` on `security.capability`

BuildKit's containerd image store exports layers by mounting an overlay
in a tmpmount directory and walking files to build a tar diff. For each
file it calls `lgetxattr(…, "security.capability")`. On this kernel
those calls returned `EOPNOTSUPP`, which BuildKit treats as fatal:

> `failed to create diff tar stream: failed to get xattr for
>  /tmp/containerd-mount…/app: operation not supported`

Two causes stacked on top of each other:

- `CONFIG_SECURITY` was `n` in the x86_64 config. That disables the LSM
  framework, which is what services reads of `security.*` namespace.
  Without it, no filesystem can read/write `security.capability`. Fixed
  by flipping `CONFIG_SECURITY=y` in both configs.
- Even with `CONFIG_SECURITY=y`, arm64 still failed — because the arm64
  config never enabled `CONFIG_EXT4_FS_SECURITY`, so ext4 specifically
  had no security xattr handler. All `security.*` reads on ext4 files
  returned `EOPNOTSUPP`. Fixed by adding `EXT4_FS_SECURITY=y` (and
  `EXT4_FS_POSIX_ACL=y` alongside it) to the arm64 config. x86_64
  already had both.

The diagnostic path was: `strace -e trace=…getxattr` on dockerd →
isolate the failing syscall → prove it's the xattr namespace, not a
specific file → python one-liner showing `os.setxattr(…,
'security.capability', …)` on `/tmp/cap-test` (tmpfs) worked but on
`/var/lib/docker/cap-test` (ext4) returned `[Errno 95] Not supported`
→ `zcat /proc/config.gz | grep EXT4_FS_SECURITY` → `not set`.

### (c) BuildKit diff overlays mounted on `/tmp` inherited outer xattr quirks

BuildKit also mounts transient overlays at `/tmp/containerd-mount*/`
(distinct from the ones at `/var/lib/docker/containerd/daemon/tmpmounts`
— different code path). `/tmp` in the container was previously just a
directory on the outer overlayfs rootfs, which is mounted with
`userxattr` (required by the host-side OCI extractor — see
`app/airlockd/src/init/linux/overlay.rs:113`). The inner overlay
wouldn't always cleanly round-trip `security.*` reads through a stacked
overlay-on-userxattr-overlay path, depending on lowerdir placement.

Mounting `/tmp` as a dedicated tmpfs eliminates the outer-overlay
interaction entirely, matches how most Linux distros set up `/tmp`
anyway, and gives build scratch space its own fs boundary. `noexec` is
intentionally omitted — build tools execute scripts from `/tmp`.

## Overlayfs kernel features

While we were there, also enabled `OVERLAY_FS_INDEX`,
`OVERLAY_FS_REDIRECT_DIR`, `OVERLAY_FS_METACOPY`, `OVERLAY_FS_XINO_AUTO`
on both arches. These are defaults-only knobs that Docker's overlay2
graphdriver and nested-overlay setups benefit from. They didn't fix any
specific symptom we hit, but without them some Docker features (e.g.
overlay copy-up optimizations for dirs with redirected paths) silently
fall back to slower paths.

## Why config-arm64 is a defconfig fragment and config-x86_64 is a full
`.config`

Not touched here, but worth noting for future readers: the x86_64 config
is a full auto-generated `.config` (every option either set or explicitly
`not set`), while the arm64 config is a minimal list of `=y` overrides
on top of the arm64 defconfig. That's why `EXT4_FS_SECURITY` existed in
x86_64 but had to be added explicitly to arm64 — the defconfig default
is off. Anyone adding a new `CONFIG_…=y` needs to update both files.

## Files touched

- `app/vm-kernel/config-x86_64` — netfilter xtables targets/matches,
  overlay sub-features, `CONFIG_SECURITY=y`.
- `app/vm-kernel/config-arm64` — same set, plus `CONFIG_EXT4_FS_SECURITY`
  and `CONFIG_EXT4_FS_POSIX_ACL` which x86_64 already had.
- `app/airlockd/src/init/linux/container.rs` — tmpfs mount at `/tmp` in
  the container rootfs, right after `/dev/shm`.

## Follow-ups / related

- Option 2 of `docs/manual/src/tips/docker.md` is unchanged — the
  `/airlock/disk/docker` → `/var/lib/docker` bind mount the user must
  still do by hand — but the `containerd-snapshotter: false` workaround
  we briefly recommended during debugging is no longer needed.
