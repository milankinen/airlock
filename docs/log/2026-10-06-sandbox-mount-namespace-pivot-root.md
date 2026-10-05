# Enter the container rootfs through a pivot_root'ed mount namespace

## Problem

`docker exec` inside a sandbox ran in the VM's initramfs, not in the Docker
container. Docker healthchecks (which use exec) always failed, and any
`setns`-based entry (`runc exec`, `nsenter -m`, CRI exec) could see the VM
shares (`/mnt/project`, `/mnt/disk`, `/mnt/files`).

airlockd entered the container with `chroot("/mnt/overlay/rootfs")`. That only
changes the calling process's root. The mount namespace root stayed the VM's
initramfs `rootfs`. `setns(CLONE_NEWNS)` puts the joining process at the
*namespace* root, so runc's nsenter landed in the VM. `docker run` worked only
because its init process inherited the chroot.

## Why `pivot_root` was not used before

The 2026-04-01 log blamed VirtioFS ("FUSE-based, doesn't support
`pivot_root`"). That is not the cause, and the rootfs is overlayfs nowadays
anyway. The real obstacle is the initramfs: `pivot_root(2)` returns `EINVAL`
when the caller's root mount has no parent, and the initramfs `rootfs` is the
namespace's root mount. So `pivot_root` straight from the VM root can never
work, which is also why runc/crun needed `--no-pivot` (Docker's
`DOCKER_RAMDISK`) when run directly on the initramfs.

## Change

- `app/airlockd/src/sandbox_ns.rs` (new): after `init::setup` has mounted the
  whole rootfs, a forked helper builds the sandbox mount namespace:
  1. `unshare(CLONE_NEWNS)`, make `/` private.
  2. Recursively bind the VM root onto `/mnt/.sandbox-stage`, `MS_MOVE` that
     bind onto `/` and `chroot` into it. The process root is now a mount
     *with* a parent (the initramfs), so `pivot_root` accepts it.
  3. `chdir` to the rootfs, `pivot_root(".", ".")`, `umount2(".",
     MNT_DETACH)` — the same sequence runc uses. The rootfs takes the bind's
     place on top of `/`, and the old root with every VM mount is detached.

  airlockd keeps `/proc/<helper>/ns/mnt` open and the helper exits. Failures
  report the step and errno.
- `process.rs` `build_pre_exec`: spawns `setns` into that namespace instead of
  `chroot`ing. The hardening `unshare(CLONE_NEWNS)` now runs after the setns,
  so hardened processes still get a private copy of the sandbox namespace.
  `chroot` stays as a fallback if the namespace could not be created.
- `init/linux.rs`: step 9 creates the namespace; failure logs a warning and
  falls back to chroot.

`setns` resolves the namespace root with `LOOKUP_DOWN`, which follows mounts
stacked on `/`. A process joining the namespace — including `docker exec` via
runc's nsenter — therefore gets the container rootfs as root. No VM boot
change is needed, so both VM backends stay untouched.

Compared to a plain `switch_root` (`MS_MOVE` the rootfs onto `/` + `chroot`),
which was the first version of this fix, `pivot_root` + detach leaves no VM
mounts in the sandbox namespace at all, not even unreachable ones.

Non-hardened processes (the main shell and daemons such as dockerd with
`harden = false`) now share one mount namespace, so mounts dockerd makes are
visible to them. airlockd itself stays in the VM namespace and keeps using
the `/mnt/overlay/rootfs` paths; files are shared through the same
filesystems, only mounts made inside the sandbox are not.

## Notes

- This is not a security boundary against root in the sandbox: without a PID
  namespace, root can still `nsenter -t 1 -m` into the VM namespace. The VM is
  the isolation boundary.
- No unit test: exercising the helper needs root and real mounts. The syscall
  sequence was verified manually in a dev sandbox (whose root is likewise
  stacked on a parent mount): `setns` into the result saw only the fake
  rootfs and its submounts in `/` and `mountinfo`.

## Tests

`tests/vm/mounts.bats`: `nsenter --mount=/proc/self/ns/mnt` from the sandbox
must not see `/mnt/overlay`.
