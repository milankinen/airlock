# Remove crun, add Docker support, rename VM artifacts

## Remove crun / direct process spawn

The supervisor previously launched the container process by running `/ez/oci-run`
(a shell script that invoked `crun` with a generated OCI bundle). This required
crun to be present in the container image and added an extra layer of indirection
that made error diagnostics harder.

The new approach sends full process configuration (cmd, args, env, cwd, uid, gid,
harden flag) directly in the `Supervisor.start()` RPC call. The supervisor
spawns the process directly via `spawn_user()` which:
- `chroot`s into `/mnt/overlay/rootfs`
- Sets `PR_SET_NO_NEW_PRIVS` (hardened mode)
- Performs best-effort namespace isolation (`unshare CLONE_NEWNS/NEWIPC/NEWUTS`)
- `setgid` + `setuid` to the container user

A diagnostic pipe (`pipe2(O_CLOEXEC)`) is used to pass a static tag string from
the `pre_exec` hook back to the parent on failure, so errors like
`chroot: Invalid argument` are replaced with `chroot(/mnt/overlay/rootfs): Invalid argument`.

The `oci/config.rs` module (OCI spec deserialization) was inlined into `oci.rs`
and simplified — we only need cmd, args, env, cwd, uid, gid from the spec.

`build_command()`/`build_exec_command()` in `rpc/supervisor.rs` are removed;
the host side now reads the OCI bundle and passes its contents directly over RPC.

## Docker kernel config additions (arm64)

Running Docker inside the VM failed at several points due to missing kernel configs.
The x86_64 config (built from a larger defconfig) already had most of these; the
minimised arm64 config was missing them.

Root cause pattern: `make olddefconfig` silently drops options whose dependencies
aren't satisfied. Adding the dependency option unlocks the dependent ones.

Changes to `vm/kernel/config-arm64`:
- `CONFIG_SYSVIPC=y` — unlocks `CONFIG_IPC_NS` (was silently dropped despite
  being in the config; `unshare(CLONE_NEWIPC)` also now works)
- `CONFIG_BPF=y` + `CONFIG_BPF_SYSCALL=y` — required by `CONFIG_CGROUP_BPF`;
  without these the `bpf()` syscall returns ENOSYS, breaking runc's
  `bpf_prog_query(BPF_CGROUP_DEVICE)` call
- `CONFIG_BLK_CGROUP=y` — enables the cgroupv2 `io` controller
- `CONFIG_NETFILTER_ADVANCED=y` — unlocks `IP_NF_FILTER`, `IP_NF_NAT`, etc.
  (all previously dropped silently)
- `CONFIG_IP_NF_MANGLE=y`, `CONFIG_IP_NF_RAW=y`, `CONFIG_IP_NF_TARGET_MASQUERADE=y`
- `CONFIG_NFT_MASQ=y`, `CONFIG_NETFILTER_XT_TARGET_MASQUERADE=y` — MASQUERADE
  target for both nftables and xtables backends (Alpine 1.8.11 uses nft backend)
- `CONFIG_NETFILTER_XT_MARK=y`
- `CONFIG_POSIX_MQUEUE=y`, `CONFIG_KEYS=y` — misc "Generally Necessary" items
- `CONFIG_SECCOMP=y` + `CONFIG_SECCOMP_FILTER=y` (was explicitly disabled before)

x86_64 additions: `CONFIG_NETFILTER_ADVANCED=y`, `CONFIG_NETFILTER_XT_MARK=y`,
`CONFIG_IP_NF_MANGLE=y`, `CONFIG_IP_NF_RAW=y`, `CONFIG_IP_NF_TARGET_MASQUERADE=y`.

## /ez path reorganisation and /ez/disk

Docker's overlayfs snapshotter fails when `/var/lib/docker` is on the VM's
overlayfs rootfs: the kernel rejects overlay mounts whose upper/work dirs are
on overlayfs when the inner mount uses `index=off` (which containerd hardcodes).

Rather than hardcoding Docker-specific logic in the supervisor, the supervisor
now exposes `/ez/disk` inside the container — a bind mount of `/mnt/disk/userdata`
(ext4, persistent) or a tmpfs fallback when no project disk is available.
Workloads needing a non-overlayfs filesystem can bind-mount a subdirectory:
```sh
mkdir -p /ez/disk/docker && mount --bind /ez/disk/docker /var/lib/docker
```

VirtioFS file-mount staging dirs moved from `/.ez/files_rw` / `/.ez/files_ro`
to `/ez/.files/rw` / `/ez/.files/ro` for consistency with the new `/ez/` namespace.

## Rename vm/rootfs → vm/initramfs, build:supervisor → build:ezd

`vm/rootfs/` held the initramfs build script and init, not a rootfs — renamed to
`vm/initramfs/` to match what it actually produces (`target/vm/initramfs.gz`).

`build:supervisor` → `build:ezd` to match the binary name (`ezd`).

`rootfs.tar.gz` removed from the build: it was a leftover from the old libkrun-based
Linux path. Nothing currently uses it — only `initramfs.gz` is embedded in the CLI.
